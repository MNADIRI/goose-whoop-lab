import Charts
import Foundation
import SwiftUI

// whoop-lab: single source of every physiological value the app shows.
//
//   strap -> BLE -> decoded_frames (goose) -> whoop_lab_biometric (1 Hz, Rust)
//         -> whoop_lab.analyze (Rust, per local day, cached) -> LabResults -> Home / Health / details
//
// The Rust analysis is a port of the reference Python lab (lab/analyze.py); lab/METRICS.md maps
// every raw sensor field to its algorithm and to the screen that shows it.

// MARK: - Model (schema whoop-lab.results.v2)

struct LabResults: Decodable {
  let schema: String
  let generated: Double
  let firstDay: String?
  let days: [LabDayResult]
  let load: LabLoad?
  let device: LabDevice

  var latest: LabDayResult? { days.last }

  /// Metric screens that show these results (diagnostic screens keep goose's own reports).
  static let coveredRoutes: Set<HealthRoute> = [.healthMonitor, .sleep, .recovery, .strain, .stress, .cardioLoad, .energyBank]

  func day(_ date: String) -> LabDayResult? { days.first { $0.date == date } }
}

struct LabDayResult: Decodable, Identifiable {
  var id: String { date }
  let date: String
  let night: LabNight
  let day: LabDay
}

struct LabNight: Decodable {
  let available: Bool
  let bedTime: Double?
  let sleepOnset: Double?
  let wakeTime: Double?
  let timeInBedMin: Double?
  let asleepMin: Double?
  let latencyMin: Double?
  let efficiencyPct: Double?
  let awakenings: Int?
  let stagesMin: [String: Double]?
  let restingHrBpm: Double?
  let meanSleepHrBpm: Double?
  let hrvNight: LabHRV?
  let hrvLastThird: LabHRV?
  let respiration: LabRespiration?
  let skinTempC: Double?
  let skinTempDeltaC: Double?
  let recovery: LabRecovery?
  let hypnogram: String?
  let hypnogramStart: Double?

  /// Sleep performance: hours slept vs an 8 h need.
  var sleepScore: Int? { asleepMin.map { min(100, Int(($0 / 480 * 100).rounded())) } }
}

struct LabHRV: Decodable {
  let rmssdMs: Double
  let lnRmssd: Double
  let sdnnMs: Double
  let pnn50Pct: Double
  let successiveDiffs: Int
}

struct LabRespiration: Decodable {
  let rpm: Double
  let rpmIqr: [Double]
  let windows: Int
}

struct LabRecovery: Decodable {
  let score: Double?
  let reason: String?
  let zLnRmssd: Double?
  let zRestingHr: Double?
}

struct LabStress: Decodable {
  let mean: Double
  let max: Double
  let minutes: [String: Double]
  let buckets: Int
}

struct LabDay: Decodable {
  struct Energy: Decodable {
    let restingKcal: Double
    let activeKcal: Double
    let totalKcal: Double
    let profileDefaultsUsed: Bool
  }
  struct Hour: Decodable {
    let hr: Double
    let steps: Int
  }
  let coverageH: Double
  let steps: Int?
  let walkMin: Double?
  let runMin: Double?
  let mvpaMin: Double?
  let hrZoneMin: [String: Double]?
  let edwardsTrimp: Double?
  let strain021: Double?
  let energy: Energy?
  let hrMean: Double?
  let hrMin: Double?
  let hrMax: Double?
  let hourly: [String: Hour]?
  let stress: LabStress?

  // convertFromSnakeCase turns "strain_0_21" into "strain021".
  enum CodingKeys: String, CodingKey {
    case coverageH, steps, walkMin, runMin, mvpaMin, hrZoneMin, edwardsTrimp, energy, hrMean, hrMin, hrMax, hourly, stress
    case strain021 = "strain021"
  }

  var hasActivity: Bool { steps != nil }
}

struct LabLoad: Decodable {
  struct Day: Decodable {
    let date: String
    let trimp: Double
  }
  let acute7d: Double
  let chronic28dWeekly: Double
  let acwr: Double?
  let days: Int
  let daily: [Day]

  // convertFromSnakeCase turns "acute_7d" into "acute7D" (capitalised word "7d").
  enum CodingKeys: String, CodingKey {
    case acute7d = "acute7D"
    case chronic28dWeekly = "chronic28DWeekly"
    case acwr, days, daily
  }
}

struct LabDevice: Decodable {
  let records: Int
  let coveragePct: Double?
}

// MARK: - Store

/// Runs the on-device analysis (Rust bridge) off the main thread and publishes the results.
@MainActor
final class WhoopLabResultsStore: ObservableObject {
  @Published private(set) var results: LabResults?
  @Published private(set) var status = "Analyse en attente"
  @Published private(set) var isRunning = false
  private var lastRunAt: Date?
  private let queue = DispatchQueue(label: "whoop-lab.analysis", qos: .utility)

  /// `force` bypasses the 20 s spacing (pull-to-refresh, end of a history sync).
  func refresh(force: Bool = false) {
    guard !isRunning else { return }
    if !force, let lastRunAt, Date().timeIntervalSince(lastRunAt) < 20 { return }
    isRunning = true
    let args = Self.arguments()
    queue.async { [weak self] in
      let outcome = Self.run(args: args)
      Task { @MainActor in
        guard let self else { return }
        self.isRunning = false
        self.lastRunAt = Date()
        switch outcome {
        case .success(let results):
          self.results = results
          self.status = "Analysé \(Date().formatted(date: .omitted, time: .shortened))"
        case .failure(let error):
          self.status = "Analyse impossible : \(error.localizedDescription)"
        }
      }
    }
  }

  private static func arguments() -> [String: Any] {
    let profile = OnboardingProfileSnapshot()
    var p: [String: Any] = [:]
    if let age = HealthDataStore.profileAgeYears(from: profile.dateOfBirthString, calendar: .current) { p["age"] = age }
    if let sex = HealthDataStore.normalizedProfileSex(profile.genderRaw) { p["sex"] = sex }
    if profile.weightGrams > 0 { p["weight_kg"] = Double(profile.weightGrams) / 1000 }
    if profile.heightMm > 0 { p["height_cm"] = Double(profile.heightMm) / 10 }
    return [
      "database_path": HealthDataStore.defaultDatabasePath(),
      "utc_offset_seconds": TimeZone.current.secondsFromGMT(),
      "profile": p,
    ]
  }

  nonisolated private static func run(args: [String: Any]) -> Result<LabResults, Error> {
    do {
      let value = try GooseRustBridge().request(method: "whoop_lab.analyze", args: args)
      let data = try JSONSerialization.data(withJSONObject: value)
      let decoder = JSONDecoder()
      decoder.keyDecodingStrategy = .convertFromSnakeCase
      return .success(try decoder.decode(LabResults.self, from: data))
    } catch {
      return .failure(error)
    }
  }
}

// MARK: - Formatting

func labFmt(_ value: Double?, _ digits: Int = 0) -> String {
  guard let value, value.isFinite else { return "—" }
  return value.formatted(.number.precision(.fractionLength(digits)))
}

func labDuration(_ minutes: Double?) -> String {
  guard let minutes else { return "—" }
  return "\(Int(minutes) / 60) h \(String(format: "%02d", Int(minutes) % 60))"
}

func labClock(_ ts: Double?) -> String {
  guard let ts else { return "—" }
  return Date(timeIntervalSince1970: ts).formatted(date: .omitted, time: .shortened)
}

func labDayLabel(_ date: String, style: Date.FormatStyle = .dateTime.weekday(.wide).day().month(.wide)) -> String {
  guard let parsed = labDate(date) else { return date }
  return parsed.formatted(style).capitalized
}

func labDate(_ key: String) -> Date? {
  let f = DateFormatter()
  f.locale = Locale(identifier: "en_US_POSIX")
  f.dateFormat = "yyyy-MM-dd"
  return f.date(from: key)
}

// MARK: - Building blocks

struct LabTile: View {
  let title: String
  let value: String
  var unit: String = ""
  var note: String = ""
  var tint: Color = .primary

  var body: some View {
    VStack(alignment: .leading, spacing: 3) {
      Text(title).font(.caption).foregroundStyle(.secondary)
      HStack(alignment: .firstTextBaseline, spacing: 3) {
        Text(value).font(.title2.weight(.semibold)).monospacedDigit().foregroundStyle(tint)
        if !unit.isEmpty { Text(unit).font(.caption).foregroundStyle(.secondary) }
      }
      if !note.isEmpty { Text(note).font(.caption2).foregroundStyle(.secondary).lineLimit(2) }
    }
    .frame(maxWidth: .infinity, alignment: .leading)
    .padding(12)
    .background(.background.secondary, in: RoundedRectangle(cornerRadius: 14))
  }
}

struct LabSectionHeader: View {
  let title: String
  var subtitle: String = ""

  var body: some View {
    VStack(alignment: .leading, spacing: 2) {
      Text(title).font(.title3.weight(.semibold))
      if !subtitle.isEmpty { Text(subtitle).font(.caption).foregroundStyle(.secondary) }
    }
  }
}

let labColumns = [GridItem(.flexible(), spacing: 10), GridItem(.flexible(), spacing: 10)]

// MARK: - Day sections (Home)

struct LabScoreRow: View {
  let night: LabNight
  let day: LabDay

  var body: some View {
    HStack(spacing: 10) {
      ring("Sommeil", value: night.sleepScore.map(Double.init), max: 100, text: night.sleepScore.map { "\($0)%" } ?? "—",
           note: night.available ? labDuration(night.asleepMin) : "pas de nuit", color: .indigo)
      ring("Récupération", value: night.recovery?.score, max: 100, text: night.recovery?.score.map { "\(Int($0))%" } ?? "—",
           note: night.recovery?.score == nil ? (night.recovery?.reason ?? "pas de nuit") : "vs ta référence", color: .green)
      ring("Effort", value: day.strain021, max: 21, text: labFmt(day.strain021, 1),
           note: "sur 21", color: .orange)
    }
  }

  private func ring(_ title: String, value: Double?, max: Double, text: String, note: String, color: Color) -> some View {
    VStack(spacing: 6) {
      ZStack {
        Circle().stroke(color.opacity(0.15), lineWidth: 8)
        Circle().trim(from: 0, to: CGFloat(min(1, (value ?? 0) / max)))
          .stroke(color, style: StrokeStyle(lineWidth: 8, lineCap: .round))
          .rotationEffect(.degrees(-90))
        Text(text).font(.headline.monospacedDigit())
      }
      .frame(width: 76, height: 76)
      Text(title).font(.caption.weight(.semibold))
      Text(note).font(.caption2).foregroundStyle(.secondary).multilineTextAlignment(.center).lineLimit(2)
    }
    .frame(maxWidth: .infinity)
  }
}

struct LabNightSection: View {
  let night: LabNight

  var body: some View {
    VStack(alignment: .leading, spacing: 10) {
      LabSectionHeader(title: "Nuit", subtitle: night.available
        ? "\(labClock(night.bedTime)) coucher · \(labClock(night.sleepOnset)) endormi · \(labClock(night.wakeTime)) réveil"
        : "Aucune nuit détectée (bracelet non porté ou pas de sommeil)")
      if night.available {
        LazyVGrid(columns: labColumns, spacing: 10) {
          LabTile(title: "Temps de sommeil", value: labDuration(night.asleepMin), note: "au lit \(labDuration(night.timeInBedMin))")
          LabTile(title: "Efficacité", value: labFmt(night.efficiencyPct), unit: "%",
                  note: "latence \(labFmt(night.latencyMin)) min · \(night.awakenings ?? 0) réveils")
        }
        if let codes = night.hypnogram, let start = night.hypnogramStart {
          LabHypnogram(codes: Array(codes), start: start, stages: night.stagesMin ?? [:])
        }
      }
    }
  }
}

struct LabBodySection: View {
  let night: LabNight

  var body: some View {
    VStack(alignment: .leading, spacing: 10) {
      LabSectionHeader(title: "Corps", subtitle: "Mesuré pendant le sommeil")
      LazyVGrid(columns: labColumns, spacing: 10) {
        LabTile(title: "FC au repos", value: labFmt(night.restingHrBpm), unit: "bpm",
                note: "moyenne nuit \(labFmt(night.meanSleepHrBpm)) bpm")
        LabTile(title: "VFC (RMSSD)", value: labFmt(night.hrvNight?.rmssdMs), unit: "ms",
                note: night.hrvNight.map { "SDNN \(labFmt($0.sdnnMs)) ms · fin de nuit \(labFmt(night.hrvLastThird?.rmssdMs)) ms" } ?? "pas assez de battements")
        LabTile(title: "Respiration", value: labFmt(night.respiration?.rpm, 1), unit: "/min",
                note: night.respiration.map { "plage \(labFmt($0.rpmIqr.first, 1))–\(labFmt($0.rpmIqr.last, 1))" } ?? "pas assez de battements")
        LabTile(title: "Température peau", value: night.skinTempDeltaC.map { String(format: "%+.1f", $0) } ?? labFmt(night.skinTempC, 1),
                unit: "°C", note: night.skinTempDeltaC != nil ? "écart à ta moyenne · \(labFmt(night.skinTempC, 1)) °C" : "écart après 3 nuits")
      }
    }
  }
}

struct LabActivitySection: View {
  let day: LabDay

  var body: some View {
    VStack(alignment: .leading, spacing: 10) {
      LabSectionHeader(title: "Activité", subtitle: "\(labFmt(day.coverageH, 1)) h de port ce jour")
      LazyVGrid(columns: labColumns, spacing: 10) {
        LabTile(title: "Pas", value: day.steps?.formatted() ?? "—", note: "compteur du bracelet")
        LabTile(title: "Calories actives", value: labFmt(day.energy?.activeKcal), unit: "kcal",
                note: "dépense totale \(labFmt(day.energy?.totalKcal)) kcal avec le métabolisme de base")
        LabTile(title: "Activité modérée+", value: labFmt(day.mvpaMin), unit: "min",
                note: "marche \(labFmt(day.walkMin)) min · course \(labFmt(day.runMin)) min")
        LabTile(title: "Stress", value: labFmt(day.stress?.mean), unit: "/100",
                note: day.stress.map { "max \(labFmt($0.max)) · éveillé et immobile" } ?? "pas assez de repos éveillé")
        LabTile(title: "FC journée", value: labFmt(day.hrMean), unit: "bpm", note: "min \(labFmt(day.hrMin)) · max \(labFmt(day.hrMax))")
        LabTile(title: "Effort", value: labFmt(day.strain021, 1), unit: "/21", note: "TRIMP \(labFmt(day.edwardsTrimp, 1))")
      }
      if let hourly = day.hourly, !hourly.isEmpty {
        let points = hourly.compactMap { key, value in Int(key).map { ($0, value) } }.sorted { $0.0 < $1.0 }
        Text("Pas par heure").font(.caption).foregroundStyle(.secondary)
        Chart(points, id: \.0) { hour, value in
          BarMark(x: .value("Heure", hour), y: .value("Pas", value.steps)).foregroundStyle(.orange)
        }
        .chartXScale(domain: 0...23)
        .frame(height: 110)
        Text("Fréquence cardiaque par heure").font(.caption).foregroundStyle(.secondary)
        Chart(points, id: \.0) { hour, value in
          LineMark(x: .value("Heure", hour), y: .value("FC", value.hr)).foregroundStyle(.red)
          PointMark(x: .value("Heure", hour), y: .value("FC", value.hr)).foregroundStyle(.red)
        }
        .chartXScale(domain: 0...23)
        .chartYScale(domain: .automatic(includesZero: false))
        .frame(height: 110)
      }
      if day.energy?.profileDefaultsUsed == true {
        Text("Profil incomplet (âge, poids) : calories et zones calculées avec des valeurs par défaut.")
          .font(.caption2).foregroundStyle(.secondary)
      }
    }
  }
}

struct LabHypnogram: View {
  let codes: [Character]
  let start: Double
  let stages: [String: Double]

  private static let rows: [(Character, String, String, Color)] = [
    ("W", "wake", "Éveil", .red), ("R", "rem", "Paradoxal", .purple), ("L", "light", "Léger", .cyan), ("D", "deep", "Profond", .indigo),
  ]

  var body: some View {
    VStack(alignment: .leading, spacing: 6) {
      Canvas { context, size in
        guard !codes.isEmpty else { return }
        let step = size.width / CGFloat(codes.count)
        let rowHeight = size.height / 4
        for (index, code) in codes.enumerated() {
          guard let row = Self.rows.firstIndex(where: { $0.0 == code }) else { continue }
          let rect = CGRect(x: CGFloat(index) * step, y: CGFloat(row) * rowHeight + 2, width: step + 0.5, height: rowHeight - 4)
          context.fill(Path(rect), with: .color(Self.rows[row].3))
        }
      }
      .frame(height: 72)
      HStack {
        Text(labClock(start)).font(.caption2).foregroundStyle(.secondary)
        Spacer()
        Text(labClock(start + Double(codes.count) * 30)).font(.caption2).foregroundStyle(.secondary)
      }
      HStack(spacing: 12) {
        ForEach(Self.rows, id: \.1) { row in
          VStack(alignment: .leading, spacing: 0) {
            Text(row.2).font(.caption2).foregroundStyle(row.3)
            Text(labDuration(stages[row.1])).font(.caption.monospacedDigit())
          }
        }
      }
      Text("Stades estimés sans EEG (FC, VFC, mouvement) ; le profond est le moins fiable.")
        .font(.caption2).foregroundStyle(.secondary)
    }
  }
}

// MARK: - Trends (Health)

struct LabTrendsView: View {
  let results: LabResults

  private struct Point: Identifiable {
    let id: String
    let date: Date
    let value: Double
  }

  var body: some View {
    VStack(alignment: .leading, spacing: 22) {
      LabSectionHeader(title: "Tendances", subtitle: "Depuis le \(labDayLabel(results.firstDay ?? "", style: .dateTime.day().month(.wide)))")
      chart("Sommeil", unit: "h", color: .indigo) { $0.night.asleepMin.map { $0 / 60 } }
      chart("FC au repos", unit: "bpm", color: .red) { $0.night.restingHrBpm }
      chart("VFC (RMSSD)", unit: "ms", color: .green) { $0.night.hrvNight?.rmssdMs }
      chart("Respiration", unit: "/min", color: .teal) { $0.night.respiration?.rpm }
      chart("Température peau", unit: "°C", color: .pink) { $0.night.skinTempC }
      chart("Pas", unit: "", color: .orange, bars: true) { $0.day.steps.map(Double.init) }
      chart("Calories actives", unit: "kcal", color: .orange, bars: true) { $0.day.energy?.activeKcal }
      chart("Effort", unit: "/21", color: .orange, bars: true) { $0.day.strain021 }
      chart("Stress", unit: "/100", color: .yellow) { $0.day.stress?.mean }
      if let load = results.load {
        LabLoadSection(load: load)
      }
    }
  }

  @ViewBuilder
  private func chart(_ title: String, unit: String, color: Color, bars: Bool = false, value: (LabDayResult) -> Double?) -> some View {
    let points = results.days.compactMap { day -> Point? in
      guard let v = value(day), let d = labDate(day.date) else { return nil }
      return Point(id: day.date, date: d, value: v)
    }
    VStack(alignment: .leading, spacing: 4) {
      HStack {
        Text(title).font(.subheadline.weight(.semibold))
        Spacer()
        Text(points.last.map { "\(labFmt($0.value, unit == "h" || unit == "/min" || unit == "°C" ? 1 : 0)) \(unit)" } ?? "—")
          .font(.subheadline.monospacedDigit()).foregroundStyle(.secondary)
      }
      if points.isEmpty {
        Text("Pas encore de donnée").font(.caption).foregroundStyle(.secondary)
      } else {
        Chart(points) { p in
          if bars {
            BarMark(x: .value("Jour", p.date, unit: .day), y: .value(title, p.value)).foregroundStyle(color)
          } else {
            LineMark(x: .value("Jour", p.date, unit: .day), y: .value(title, p.value)).foregroundStyle(color)
            PointMark(x: .value("Jour", p.date, unit: .day), y: .value(title, p.value)).foregroundStyle(color)
          }
        }
        .chartYScale(domain: .automatic(includesZero: bars))
        .frame(height: 90)
      }
    }
  }
}

struct LabLoadSection: View {
  let load: LabLoad

  var body: some View {
    VStack(alignment: .leading, spacing: 8) {
      LabSectionHeader(title: "Charge cardio", subtitle: "TRIMP d'Edwards cumulé")
      LazyVGrid(columns: labColumns, spacing: 10) {
        LabTile(title: "7 derniers jours", value: labFmt(load.acute7d), unit: "TRIMP")
        LabTile(title: "Moyenne hebdo (28 j)", value: labFmt(load.chronic28dWeekly), unit: "TRIMP")
      }
      Text(load.acwr.map { "Ratio aigu/chronique \(labFmt($0, 2)) (0,8–1,3 = charge habituelle)" }
           ?? "Ratio aigu/chronique disponible après 21 jours (\(load.days) j)")
        .font(.caption).foregroundStyle(.secondary)
    }
  }
}

struct LabMethodsView: View {
  var body: some View {
    List {
      Section("Chaîne") {
        Text("Bracelet → Bluetooth → trames brutes vérifiées (CRC) → table 1 Hz → analyse par jour sur l'iPhone → écrans. Une seule source pour toutes les valeurs.")
      }
      Section("Méthodes") {
        Text("Sommeil : état du bracelet + Cole-Kripke (te Lindert) sur la gravité au-dessus de 10 mg de bruit.")
        Text("Stades : règles transparentes FC / VFC locale / mouvement, sans EEG (≈ 65–73 % d'accord au mieux).")
        Text("FC au repos : plus basse moyenne de 5 min pendant le sommeil.")
        Text("VFC : RMSSD, SDNN, pNN50 sur battements réellement successifs (saut ≤ 20 %).")
        Text("Respiration : arythmie sinusale respiratoire, pic 0,13–0,45 Hz sur fenêtres de 2 min.")
        Text("Température : médiane de la peau pendant le sommeil, écart à la moyenne des nuits précédentes (≥ 3).")
        Text("Récupération : z-scores de ln(RMSSD) et de la FC au repos vs ta référence (≥ 3 nuits).")
        Text("Pas : incréments du compteur du bracelet. Il compte au poignet, Apple Health compte au téléphone : les totaux diffèrent.")
        Text("Effort : TRIMP d'Edwards sur zones de %FCmax (Tanaka), ramené sur 0–21. Charge : 7 j vs 28 j.")
        Text("Calories : actives = Keytel 2005 au-dessus de FC repos + 20 ; totales = + métabolisme de base (Mifflin-St Jeor).")
        Text("Stress : FC au-dessus du repos, éveillé et immobile, rapportée à 40 % de la réserve cardiaque.")
      }
      Section("Non disponible") {
        Text("SpO₂ : aucun champ fiable dans les trames du bracelet ; rien n'est inventé.")
      }
    }
    .navigationTitle("Méthodes")
  }
}

/// Detail screen for a metric family: same sections as Home for the latest day, plus its trend.
struct LabRouteDetailView: View {
  let route: HealthRoute
  let results: LabResults

  var body: some View {
    ScrollView {
      VStack(alignment: .leading, spacing: 22) {
        if let day = results.latest {
          switch route {
          case .sleep, .recovery:
            LabScoreRow(night: day.night, day: day.day)
            LabNightSection(night: day.night)
            if day.night.available { LabBodySection(night: day.night) }
          case .healthMonitor:
            if day.night.available { LabBodySection(night: day.night) }
          case .strain, .stress, .cardioLoad, .energyBank:
            LabActivitySection(day: day.day)
            if let load = results.load { LabLoadSection(load: load) }
          default:
            EmptyView()
          }
        }
        LabTrendsView(results: results)
      }
      .padding()
    }
    .navigationTitle(route.title)
  }
}

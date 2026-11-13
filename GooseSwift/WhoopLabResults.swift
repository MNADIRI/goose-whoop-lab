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
  let circadian: LabCircadian?

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
  let indices: LabIndices?
}

/// Composite indices computed over the days up to this one (health_indices.rs).
struct LabIndices: Decodable {
  struct Score: Decodable { let score: Double?; let reason: String? }
  struct Signal: Decodable { let signal: String }
  struct Warning: Decodable { let level: String; let signals: [Signal]?; let overreachingSuspected: Bool? }
  struct VO2: Decodable { let mlKgMin: Double? }
  let fatigue: Score?
  let recovery: Score?
  let earlyWarning: Warning?
  let vo2max: VO2?
}

struct LabCircadian: Decodable {
  struct Cos: Decodable { let acrophaseH: Double?; let troughH: Double?; let amplitude: Double? }
  let heartRate: Cos?
  let skinTemp: Cos?
  let sleepRegularityIndex: Double?
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
  let spo2: LabSpO2?
  let firmwareRestingHr: FirmwareRHR?
  let perfusion: Perfusion?
  let morningHrv: MorningHRV?
  let cpc: CPC?
  let hrvSpectral: Spectral?
  let breathingRegularity: Breathing?
  let respiratoryDisturbance: RespDist?
  let movement: Movement?
  let skinOnsetWarmingC: Double?

  struct MorningHRV: Decodable { let available: Bool; let rmssdMs: Double? }
  struct CPC: Decodable { let stablePct: Double?; let unstablePct: Double? }
  struct Spectral: Decodable { struct Night: Decodable { let lfHf: Double?; let hfNu: Double? }; let night: Night? }
  struct Breathing: Decodable { let rateCvPct: Double?; let irregularWindowsPct: Double? }
  struct RespDist: Decodable { let index0100: Double?; let hrSurgesPerH: Double?; let desatWindows: Int? }
  struct Movement: Decodable { let postureChangesPerH: Double?; let episodesPerH: Double? }

  struct FirmwareRHR: Decodable { let medianBpm: Double?; let p10Bpm: Double? }
  struct Perfusion: Decodable { let opticalAmplitudeMedian: Double?; let vsPreviousNightsPct: Double? }
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

/// On-strap SpO2 (R18 byte 82): 30 s spot windows every 20 min during sleep. Not validated.
struct LabSpO2: Decodable {
  let available: Bool
  let medianPct: Double?
  let minWindowPct: Double?
  let validWindows: Int?
  let windows: Int?
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
  let cadence: Cadence?
  let sedentary: Sedentary?

  struct Sedentary: Decodable { let sedentaryInBoutsMin: Double?; let longestBoutMin: Double? }

  struct Cadence: Decodable {
    let available: Bool
    let medianSpm: Double?
    let p90Spm: Double?
  }

  // convertFromSnakeCase turns "strain_0_21" into "strain021".
  enum CodingKeys: String, CodingKey {
    case coverageH, steps, walkMin, runMin, mvpaMin, hrZoneMin, edwardsTrimp, energy, hrMean, hrMin, hrMax, hourly, stress, cadence, sedentary
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
  /// The same analysis as plain JSON: every field the typed model does not decode (Forme screens).
  @Published private(set) var raw: [String: Any] = [:]
  @Published private(set) var status = "Analyse en attente"
  @Published private(set) var isRunning = false
  private var lastRunAt: Date?
  private let queue = DispatchQueue(label: "whoop-lab.analysis", qos: .utility)

  /// The last analysis, kept on disk so the screens open on it instantly (no "analysis running"
  /// screen at launch); the fresh analysis replaces it silently a moment later.
  nonisolated private static var cacheURL: URL? {
    FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask).first?
      .appendingPathComponent("GooseSwift/forme_last_analysis.json")
  }

  init() {
    guard let url = Self.cacheURL, let data = try? Data(contentsOf: url),
          let raw = try? JSONSerialization.jsonObject(with: data) as? [String: Any] else { return }
    self.raw = raw
    let decoder = JSONDecoder()
    decoder.keyDecodingStrategy = .convertFromSnakeCase
    self.results = try? decoder.decode(LabResults.self, from: data)
    self.status = "Dernière analyse"
  }

  /// `force` bypasses the 20 s spacing (pull-to-refresh, end of a history sync).
  func refresh(force: Bool = false) {
    #if targetEnvironment(simulator)
    // simulator only: Documents/forme_preview.json (a real analysis copied in) drives the screens
    if let dir = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask).first,
       let data = try? Data(contentsOf: dir.appendingPathComponent("forme_preview.json")),
       let raw = try? JSONSerialization.jsonObject(with: data) as? [String: Any] {
      self.raw = raw
      let decoder = JSONDecoder()
      decoder.keyDecodingStrategy = .convertFromSnakeCase
      self.results = try? decoder.decode(LabResults.self, from: data)
      status = "Aperçu simulateur"
      return
    }
    #endif
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
        case .success(let (results, raw)):
          self.results = results
          self.raw = raw
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

  nonisolated private static func run(args: [String: Any]) -> Result<(LabResults, [String: Any]), Error> {
    do {
      let value = try GooseRustBridge().request(method: "whoop_lab.analyze", args: args)
      let data = try JSONSerialization.data(withJSONObject: value)
      if let url = cacheURL {
        try? FileManager.default.createDirectory(at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
        try? data.write(to: url, options: .atomic)
      }
      let decoder = JSONDecoder()
      decoder.keyDecodingStrategy = .convertFromSnakeCase
      return .success((try decoder.decode(LabResults.self, from: data), value))
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

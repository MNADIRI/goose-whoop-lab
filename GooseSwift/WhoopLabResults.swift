import Charts
import Foundation
import SwiftUI

// whoop-lab: results computed by the Mac lab (lab/report.py) and written to
// <export folder>/results/latest.json (schema whoop-lab.results.v1). The app shows exactly the
// numbers the lab computed: sleep, stages, HRV, respiration, temperature, activity, energy, recovery.

struct LabResults: Decodable {
  let schema: String
  let generated: Double
  let lastNight: LabNight?
  let today: LabDay?
  let nights: [LabNight]
  let load: LabLoad?
  let device: LabDevice

  /// Routes whose screens show lab values (diagnostic screens keep goose's own reports).
  static let coveredRoutes: Set<HealthRoute> = [.healthMonitor, .sleep, .recovery, .strain, .stress, .cardioLoad, .energyBank]

  func cardioLoadDays() -> [CardioLoadDay]? {
    guard let daily = load?.daily, !daily.isEmpty else { return nil }
    let peak = max(daily.map(\.trimp).max() ?? 1, 1)
    return daily.suffix(7).map { day in
      CardioLoadDay(id: day.date, dateLabel: String(day.date.suffix(5)), load: day.trimp, status: "TRIMP",
                    durationText: "", percent: day.trimp / peak, source: .deviceSensor("whoop-lab TRIMP Edwards"))
    }
  }
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

struct LabStress: Decodable {
  let mean: Double
  let max: Double
  let minutes: [String: Double]
  let buckets: Int
}

struct LabNight: Decodable, Identifiable {
  var id: String { date }
  let date: String
  let available: Bool
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
  let score: Int?
  let reason: String?
  let zLnRmssd: Double?
  let zRestingHr: Double?
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
  let date: String
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
  let hrMin: Int?
  let hrMax: Int?
  let hourly: [String: Hour]?
  let stress: LabStress?

  enum CodingKeys: String, CodingKey {
    case date, coverageH, steps, walkMin, runMin, mvpaMin, hrZoneMin, edwardsTrimp, energy, hrMean, hrMin, hrMax, hourly, stress
    case strain021 = "strain021"
  }
}

struct LabDevice: Decodable {
  struct Optical: Decodable {
    let n: Int
    let acceptedPct: Double
  }
  let coveragePct: Double
  let records: Int
  let opticalWindows: Optical
}

@MainActor
final class WhoopLabResultsStore: ObservableObject {
  @Published private(set) var results: LabResults?
  @Published private(set) var status = "Not loaded"
  private var lastModified: Date?

  /// Reads results/latest.json from the export folder (asks iCloud to download it if needed).
  func refresh(folder: URL?) {
    guard let folder else {
      status = "Choose the export folder in More → Connection → whoop-lab"
      return
    }
    Task.detached(priority: .utility) { [weak self] in
      let outcome = Self.load(folder: folder)
      await MainActor.run {
        guard let self else { return }
        switch outcome {
        case .success(let (results, modified)):
          if modified != self.lastModified || self.results == nil {
            self.results = results
            self.lastModified = modified
          }
          self.status = "Updated \(Self.relative(Date(timeIntervalSince1970: results.generated)))"
        case .failure(let error):
          self.status = error.localizedDescription
        }
      }
    }
  }

  nonisolated private static func load(folder: URL) -> Result<(LabResults, Date?), Error> {
    let accessing = folder.startAccessingSecurityScopedResource()
    defer { if accessing { folder.stopAccessingSecurityScopedResource() } }
    let url = folder.appendingPathComponent("results/latest.json")
    try? FileManager.default.startDownloadingUbiquitousItem(at: url)
    var coordinationError: NSError?
    var result: Result<(LabResults, Date?), Error> = .failure(CocoaError(.fileNoSuchFile))
    NSFileCoordinator().coordinate(readingItemAt: url, options: [], error: &coordinationError) { target in
      do {
        let data = try Data(contentsOf: target)
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        let decoded = try decoder.decode(LabResults.self, from: data)
        let modified = try? target.resourceValues(forKeys: [.contentModificationDateKey]).contentModificationDate
        result = .success((decoded, modified))
      } catch {
        result = .failure(error)
      }
    }
    if let coordinationError {
      return .failure(coordinationError)
    }
    return result
  }

  nonisolated static func relative(_ date: Date) -> String {
    let formatter = RelativeDateTimeFormatter()
    formatter.unitsStyle = .short
    return formatter.localizedString(for: date, relativeTo: Date())
  }
}

// MARK: - Home integration
// The lab is the single source for these metrics; Home overlays them on goose's snapshots and adds
// the night/day detail sections. Without lab results the app keeps its own values.

extension LabResults {
  private func make(_ s: HealthMetricSnapshot, value: String, unit: String, status: String, freshness: String,
                    detail: String, title: String? = nil) -> HealthMetricSnapshot {
    HealthMetricSnapshot(id: s.id, route: s.route, group: s.group, title: title ?? s.title, value: value, unit: unit,
                         status: status, freshness: freshness, provenance: "whoop-lab (Mac) · \(detail)",
                         source: .deviceSensor("whoop-lab \(detail)"), systemImage: s.systemImage, tint: s.tint, trend: s.trend)
  }

  func overlay(_ s: HealthMetricSnapshot) -> HealthMetricSnapshot {
    let night = lastNight?.available == true ? lastNight : nil
    let nightLabel = night.map { "Nuit du \($0.date)" } ?? ""
    switch (s.route, s.id) {
    case (.sleep, _):
      guard let n = night, let asleep = n.asleepMin else { return s }
      let performance = min(100, Int((asleep / 480 * 100).rounded()))
      return make(s, value: "\(performance)", unit: "%", status: "\(duration(asleep)) · efficacité \(fmt(n.efficiencyPct)) %",
                  freshness: nightLabel, detail: "sommeil / besoin de 8 h")
    case (.recovery, _):
      guard let n = night, let rec = n.recovery else { return s }
      if let score = rec.score {
        return make(s, value: "\(score)", unit: "%", status: "ln RMSSD z \(fmt(rec.zLnRmssd, 1)) · FC repos z \(fmt(rec.zRestingHr, 1))",
                    freshness: nightLabel, detail: "écart à ta référence")
      }
      return make(s, value: "--", unit: "%", status: rec.reason ?? "Référence en cours", freshness: nightLabel,
                  detail: "il faut 3 nuits de référence")
    case (.strain, _):
      guard let d = today, let strain = d.strain021 else { return s }
      return make(s, value: fmt(strain, 1), unit: "", status: "TRIMP \(fmt(d.edwardsTrimp, 1)) · \(fmt(d.mvpaMin)) min modérée+",
                  freshness: d.date, detail: "TRIMP Edwards sur 0–21")
    case (.stress, _):
      guard let d = today, let st = d.stress else { return s }
      return make(s, value: fmt(st.mean), unit: "", status: "max \(fmt(st.max)) · \(fmt(st.minutes["moyen"].map { $0 + (st.minutes["haut"] ?? 0) })) min moyen/haut",
                  freshness: d.date, detail: "élévation de FC au repos, hors mouvement")
    case (.cardioLoad, _):
      guard let l = load else { return s }
      return make(s, value: fmt(l.acute7d), unit: "", status: l.acwr.map { "ratio aigu/chronique \(fmt($0, 2))" } ?? "ratio après 21 jours",
                  freshness: "7 jours", detail: "somme TRIMP 7 j vs moyenne 28 j")
    case (.energyBank, _):
      guard let d = today, let e = d.energy else { return s }
      return make(s, value: fmt(e.totalKcal), unit: "kcal", status: "repos \(fmt(e.restingKcal)) · actif \(fmt(e.activeKcal))",
                  freshness: d.date, detail: "Keytel 2005 + Mifflin-St Jeor", title: "Énergie dépensée")
    case (_, "resting-hr"):
      guard let v = night?.restingHrBpm else { return s }
      return make(s, value: fmt(v), unit: "bpm", status: "Mesuré", freshness: nightLabel, detail: "plus basse moyenne 5 min du sommeil")
    case (_, "resting-hrv"):
      guard let h = night?.hrvNight else { return s }
      return make(s, value: fmt(h.rmssdMs), unit: "ms", status: "SDNN \(fmt(h.sdnnMs)) ms", freshness: nightLabel,
                  detail: "RMSSD sur battements successifs")
    case (_, "respiratory-rate"):
      guard let r = night?.respiration else { return s }
      return make(s, value: fmt(r.rpm, 1), unit: "rpm", status: "\(r.windows) fenêtres", freshness: nightLabel,
                  detail: "arythmie sinusale respiratoire")
    case (_, "wrist-temperature"):
      guard let n = night, let skin = n.skinTempC else { return s }
      if let delta = n.skinTempDeltaC {
        return make(s, value: String(format: "%+.1f", delta), unit: "°C", status: "Peau \(fmt(skin, 1)) °C", freshness: nightLabel,
                    detail: "écart à ta référence")
      }
      return make(s, value: fmt(skin, 1), unit: "°C", status: "Référence en cours", freshness: nightLabel,
                  detail: "médiane pendant le sommeil")
    case (_, "oxygen-saturation"):
      return make(s, value: "--", unit: "%", status: "Non disponible", freshness: "aucun champ fiable",
                  detail: "le bracelet n'envoie pas de SpO₂ décodable")
    default:
      return s
    }
  }
}

private struct MetricTile: View {
  let title: String
  let value: String
  let unit: String
  let note: String

  var body: some View {
    VStack(alignment: .leading, spacing: 2) {
      Text(title.uppercased()).font(.caption2).foregroundStyle(.secondary)
      HStack(alignment: .firstTextBaseline, spacing: 3) {
        Text(value).font(.title2.weight(.semibold)).monospacedDigit()
        Text(unit).font(.caption).foregroundStyle(.secondary)
      }
      Text(note).font(.caption2).foregroundStyle(.secondary).lineLimit(2)
    }
    .frame(maxWidth: .infinity, alignment: .leading)
    .padding(10)
    .background(.background.secondary, in: RoundedRectangle(cornerRadius: 12))
  }
}

private let tileColumns = [GridItem(.flexible()), GridItem(.flexible())]

private func fmt(_ value: Double?, _ digits: Int = 0) -> String {
  guard let value else { return "—" }
  return value.formatted(.number.precision(.fractionLength(digits)))
}

private func duration(_ minutes: Double?) -> String {
  guard let minutes else { return "—" }
  return "\(Int(minutes) / 60) h \(String(format: "%02d", Int(minutes) % 60))"
}

private func clock(_ ts: Double?) -> String {
  guard let ts else { return "—" }
  return Date(timeIntervalSince1970: ts).formatted(date: .omitted, time: .shortened)
}

struct LabNightSection: View {
  let night: LabNight

  var body: some View {
    VStack(alignment: .leading, spacing: 10) {
      Text("Nuit du \(night.date)").font(.title3.weight(.semibold))
      LazyVGrid(columns: tileColumns, spacing: 10) {
        MetricTile(title: "Sommeil", value: duration(night.asleepMin), unit: "",
                   note: "\(clock(night.sleepOnset)) → \(clock(night.wakeTime))")
        MetricTile(title: "Efficacité", value: fmt(night.efficiencyPct), unit: "%",
                   note: "latence \(fmt(night.latencyMin)) min · \(night.awakenings ?? 0) réveils")
        MetricTile(title: "FC au repos", value: fmt(night.restingHrBpm), unit: "bpm",
                   note: "moyenne nuit \(fmt(night.meanSleepHrBpm)) bpm")
        MetricTile(title: "VFC RMSSD", value: fmt(night.hrvNight?.rmssdMs, 1), unit: "ms",
                   note: "dernier tiers \(fmt(night.hrvLastThird?.rmssdMs, 1)) ms · SDNN \(fmt(night.hrvNight?.sdnnMs))")
        MetricTile(title: "Respiration", value: fmt(night.respiration?.rpm, 1), unit: "/min",
                   note: night.respiration.map { "IQR \(fmt($0.rpmIqr.first, 1))–\(fmt($0.rpmIqr.last, 1))" } ?? "pas assez de battements")
        MetricTile(title: "Peau", value: fmt(night.skinTempC, 1), unit: "°C",
                   note: night.skinTempDeltaC.map { "écart \(String(format: "%+.2f", $0)) °C" } ?? "référence en cours")
        MetricTile(title: "Récupération", value: night.recovery?.score.map(String.init) ?? "—", unit: "",
                   note: night.recovery?.reason ?? "z ln RMSSD \(fmt(night.recovery?.zLnRmssd, 2))")
      }
      if let hypnogram = night.hypnogram, let onset = night.sleepOnset {
        HypnogramView(codes: Array(hypnogram), onset: onset, stages: night.stagesMin ?? [:])
      }
      Text("Stades estimés sans EEG (FC, VFC, mouvement) : le sommeil profond est l'estimation la moins fiable.")
        .font(.caption2).foregroundStyle(.secondary)
    }
  }
}

private struct HypnogramView: View {
  let codes: [Character]
  let onset: Double
  let stages: [String: Double]

  private static let rows: [Character: (Int, Color, String)] = [
    "W": (0, .red, "Éveil"), "R": (1, .purple, "Paradoxal"), "L": (2, .blue.opacity(0.55), "Léger"), "D": (3, .blue, "Profond"),
  ]

  var body: some View {
    VStack(alignment: .leading, spacing: 4) {
      Canvas { context, size in
        guard !codes.isEmpty else { return }
        let step = size.width / CGFloat(codes.count)
        let rowHeight = size.height / 4
        for (index, code) in codes.enumerated() {
          guard let row = Self.rows[code] else { continue }
          let rect = CGRect(x: CGFloat(index) * step, y: CGFloat(row.0) * rowHeight + 2,
                            width: step + 0.5, height: rowHeight - 4)
          context.fill(Path(rect), with: .color(row.1))
        }
      }
      .frame(height: 80)
      HStack {
        Text(clock(onset)).font(.caption2).foregroundStyle(.secondary)
        Spacer()
        Text(clock(onset + Double(codes.count) * 30)).font(.caption2).foregroundStyle(.secondary)
      }
      HStack(spacing: 10) {
        ForEach(["W", "R", "L", "D"], id: \.self) { code in
          let row = Self.rows[Character(code)]!
          let key = ["W": "wake", "R": "rem", "L": "light", "D": "deep"][code]!
          Label("\(row.2) \(fmt(stages[key])) min", systemImage: "square.fill")
            .font(.caption2).foregroundStyle(row.1)
        }
      }
    }
  }
}

struct LabDaySection: View {
  let day: LabDay

  var body: some View {
    VStack(alignment: .leading, spacing: 10) {
      Text("Journée du \(day.date)").font(.title3.weight(.semibold))
      LazyVGrid(columns: tileColumns, spacing: 10) {
        MetricTile(title: "Pas", value: day.steps.map { $0.formatted() } ?? "—", unit: "",
                   note: "marche \(fmt(day.walkMin)) min · course \(fmt(day.runMin)) min")
        MetricTile(title: "Activité modérée+", value: fmt(day.mvpaMin), unit: "min", note: "≥ 40 % réserve cardiaque")
        MetricTile(title: "Charge cardio", value: fmt(day.strain021, 1), unit: "/21", note: "TRIMP \(fmt(day.edwardsTrimp, 1))")
        MetricTile(title: "Énergie", value: fmt(day.energy?.totalKcal), unit: "kcal",
                   note: "repos \(fmt(day.energy?.restingKcal)) · actif \(fmt(day.energy?.activeKcal))")
        MetricTile(title: "FC journée", value: fmt(day.hrMean), unit: "bpm",
                   note: "min \(day.hrMin ?? 0) · max \(day.hrMax ?? 0)")
        MetricTile(title: "Couverture", value: fmt(day.coverageH, 1), unit: "h", note: "au poignet ce jour")
      }
      if let hourly = day.hourly, !hourly.isEmpty {
        let points = hourly.compactMap { key, value in Int(key).map { ($0, value) } }.sorted { $0.0 < $1.0 }
        Chart(points, id: \.0) { hour, value in
          BarMark(x: .value("Heure", hour), y: .value("Pas", value.steps))
        }
        .chartXScale(domain: 0...23)
        .frame(height: 120)
        Chart(points, id: \.0) { hour, value in
          LineMark(x: .value("Heure", hour), y: .value("FC", value.hr))
          PointMark(x: .value("Heure", hour), y: .value("FC", value.hr))
        }
        .chartXScale(domain: 0...23)
        .frame(height: 120)
      }
      if day.energy?.profileDefaultsUsed == true {
        Text("Profil non renseigné sur le Mac (lab/profile.json) : valeurs par défaut pour l'énergie et les zones.")
          .font(.caption2).foregroundStyle(.secondary)
      }
    }
  }
}

struct LabTrendSection: View {
  let nights: [LabNight]

  var body: some View {
    VStack(alignment: .leading, spacing: 6) {
      Text("Tendances").font(.title3.weight(.semibold))
      ForEach(nights.reversed()) { night in
        HStack {
          Text(night.date).font(.caption).frame(width: 84, alignment: .leading)
          Text(duration(night.asleepMin)).font(.caption.monospacedDigit())
          Spacer()
          Text("FC \(fmt(night.restingHrBpm))").font(.caption.monospacedDigit())
          Text("RMSSD \(fmt(night.hrvNight?.rmssdMs))").font(.caption.monospacedDigit())
          Text("Resp \(fmt(night.respiration?.rpm, 1))").font(.caption.monospacedDigit())
        }
      }
      Text("Récupération et écart de température après 3 nuits de référence.").font(.caption2).foregroundStyle(.secondary)
    }
  }
}

struct LabStressSection: View {
  let stress: LabStress

  var body: some View {
    VStack(alignment: .leading, spacing: 8) {
      Text("Stress").font(.title3.weight(.semibold))
      LazyVGrid(columns: tileColumns, spacing: 10) {
        MetricTile(title: "Moyenne", value: fmt(stress.mean), unit: "/100", note: "éveillé, sans mouvement")
        MetricTile(title: "Maximum", value: fmt(stress.max), unit: "/100", note: "\(stress.buckets) tranches de 5 min")
      }
      ForEach(["repos", "bas", "moyen", "haut"], id: \.self) { band in
        HStack { Text(band.capitalized).font(.caption); Spacer(); Text("\(fmt(stress.minutes[band])) min").font(.caption.monospacedDigit()) }
      }
      Text("FC au-dessus de la FC de repos, rapportée à 40 % de la réserve cardiaque ; exercice exclu.")
        .font(.caption2).foregroundStyle(.secondary)
    }
  }
}

struct LabLoadSection: View {
  let load: LabLoad

  var body: some View {
    VStack(alignment: .leading, spacing: 8) {
      Text("Charge cardio").font(.title3.weight(.semibold))
      LazyVGrid(columns: tileColumns, spacing: 10) {
        MetricTile(title: "Aiguë 7 j", value: fmt(load.acute7d), unit: "TRIMP", note: "somme des 7 derniers jours")
        MetricTile(title: "Chronique", value: fmt(load.chronic28dWeekly), unit: "TRIMP/sem", note: "moyenne sur 28 jours")
        MetricTile(title: "Ratio A/C", value: fmt(load.acwr, 2), unit: "", note: load.acwr == nil ? "après 21 jours (\(load.days) j)" : "0,8–1,3 = zone habituelle")
      }
      Chart(load.daily, id: \.date) { day in
        BarMark(x: .value("Jour", String(day.date.suffix(5))), y: .value("TRIMP", day.trimp))
      }
      .frame(height: 120)
    }
  }
}

struct LabRouteDetailView: View {
  let route: HealthRoute
  let results: LabResults

  var body: some View {
    ScrollView {
      VStack(alignment: .leading, spacing: 20) {
        let night = results.lastNight?.available == true ? results.lastNight : nil
        switch route {
        case .sleep, .recovery, .healthMonitor:
          if let night { LabNightSection(night: night) }
          LabTrendSection(nights: results.nights.filter(\.available))
          if route == .healthMonitor { LabQualitySection(device: results.device) }
        case .stress:
          if let stress = results.today?.stress { LabStressSection(stress: stress) }
          if let day = results.today { LabDaySection(day: day) }
        case .strain, .cardioLoad, .energyBank:
          if let day = results.today { LabDaySection(day: day) }
          if let load = results.load { LabLoadSection(load: load) }
        default:
          EmptyView()
        }
        Text("Source unique : analyses whoop-lab du Mac (mêmes valeurs que Home).")
          .font(.caption2).foregroundStyle(.secondary)
      }
      .padding()
    }
  }
}

struct LabQualitySection: View {
  let device: LabDevice

  var body: some View {
    VStack(alignment: .leading, spacing: 6) {
      Text("Qualité des données").font(.title3.weight(.semibold))
      Text("Couverture 1 Hz \(fmt(device.coveragePct, 1)) % · \(device.records.formatted()) secondes")
        .font(.caption)
      Text("Fenêtres optiques acceptées \(fmt(device.opticalWindows.acceptedPct, 1)) % (\(device.opticalWindows.n))")
        .font(.caption)
      Text("SpO₂ non calculée : aucun champ fiable dans les trames du bracelet.")
        .font(.caption2).foregroundStyle(.secondary)
    }
  }
}

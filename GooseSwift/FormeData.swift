import Foundation
import SwiftUI

// MARK: - JSON access

/// Read-only path access into the analysis JSON (whoop_lab.analyze, snake_case keys).
struct J {
  let v: Any?
  init(_ v: Any?) { self.v = v }
  subscript(_ k: String) -> J { J((v as? [String: Any])?[k]) }
  subscript(_ i: Int) -> J {
    guard let a = v as? [Any], a.indices.contains(i) else { return J(nil) }
    return J(a[i])
  }
  var d: Double? {
    if let x = v as? Double { return x.isFinite ? x : nil }
    if let x = v as? Int { return Double(x) }
    if let x = v as? NSNumber { return x.doubleValue }
    return nil
  }
  var i: Int? { d.map { Int($0) } }
  var s: String? { v as? String }
  var b: Bool? { v as? Bool }
  var arr: [J] { (v as? [Any])?.map(J.init) ?? [] }
  var dict: [String: J] { ((v as? [String: Any]) ?? [:]).mapValues(J.init) }
  var exists: Bool { v != nil && !(v is NSNull) }
}

// MARK: - One day

/// Everything the Forme screens show for one day (the night that ended it + the day itself).
struct FormeDay {
  let key: String
  let date: Date
  let night: J
  let day: J
  let indices: J
  let root: J
  let index: Int
  let all: [J]

  init?(raw: [String: Any], key: String?) {
    let root = J(raw)
    let days = root["days"].arr
    guard !days.isEmpty else { return nil }
    let idx = key.flatMap { k in days.firstIndex { $0["date"].s == k } } ?? (days.count - 1)
    let d = days[idx]
    self.key = d["date"].s ?? ""
    self.date = labDate(self.key) ?? Date()
    self.night = d["night"]
    self.day = d["day"]
    self.indices = d["indices"]
    self.root = root
    self.index = idx
    self.all = days
  }

  /// Most recent day whose night was measured (the Sleep tab opens on last night, not on an empty today).
  static func latestNightKey(_ raw: [String: Any]) -> String? {
    J(raw)["days"].arr.last { $0["night"]["available"].b == true }?["date"].s
  }

  var isToday: Bool { Calendar.current.isDateInToday(date) }
  var hasNight: Bool { night["available"].b == true }

  // MARK: recovery

  var recoveryScore: Double? { indices["recovery"]["score"].d ?? night["recovery"]["score"].d }
  var recoveryReason: String? { indices["recovery"]["reason"].s ?? night["recovery"]["reason"].s }
  var hrv: Double? { night["hrv_night"]["rmssd_ms"].d }
  var hrvDelta: Double? { night["baselines"]["hrv_rmssd"]["delta"].d }
  var rhr: Double? { night["resting_hr_bpm"].d }
  var rhrDelta: Double? { night["baselines"]["resting_hr"]["delta"].d }
  var resp: Double? { night["respiration"]["rpm"].d }
  var respDelta: Double? { night["baselines"]["respiration"]["delta"].d }
  var baselineNights: Int? { night["baselines"]["hrv_rmssd"]["nights"].i }
  var baselineStatus: String? { night["baselines"]["hrv_rmssd"]["status"].s }
  var morningHRV: Double? { night["morning_hrv"]["rmssd_ms"].d }
  var fatigue: Double? { indices["fatigue"]["score"].d }
  var vo2max: Double? { indices["vo2max"]["ml_kg_min"].d }
  var banister: (atl: Double?, ctl: Double?, tsb: Double?) {
    let b = indices["fatigue"]["banister"]
    return (b["atl"].d, b["ctl"].d, b["tsb"].d)
  }

  enum Zone { case optimal, moderate, low, calibrating }
  var recoveryZone: Zone {
    guard let s = recoveryScore else { return .calibrating }
    return s >= 67 ? .optimal : (s >= 34 ? .moderate : .low)
  }
  var recoveryWord: String {
    switch recoveryZone {
    case .optimal: return "Optimale"
    case .moderate: return "Correcte"
    case .low: return "Basse"
    case .calibrating: return "Calibration"
    }
  }
  var recoverySentence: String {
    switch recoveryZone {
    case .optimal: return "Tu es bien récupéré. Une bonne journée pour pousser."
    case .moderate: return "Récupération correcte : un effort modéré te conviendra."
    case .low: return "Ton corps récupère encore : privilégie une journée légère."
    case .calibrating:
      if !hasNight { return "Pas de nuit mesurée : porte le bracelet cette nuit pour obtenir ton score." }
      if let n = baselineNights { return "Ta référence se construit (\(n) nuit\(n > 1 ? "s" : "") sur 4). Le score arrive bientôt." }
      return recoveryReason ?? "Porte le bracelet cette nuit pour obtenir ton score."
    }
  }

  // MARK: sleep

  var asleepMin: Double? { night["asleep_min"].d }
  var inBedMin: Double? { night["time_in_bed_min"].d }
  var efficiency: Double? { night["efficiency_pct"].d }
  var latency: Double? { night["latency_min"].d }
  var awakenings: Int? { night["awakenings"].i }
  var bedTime: Double? { night["bed_time"].d }
  var onset: Double? { night["sleep_onset"].d }
  var wake: Double? { night["wake_time"].d }
  var stages: [String: Double] { night["stages_min"].dict.compactMapValues { $0.d } }
  var hypnogram: String? { night["hypnogram"].s }
  var hypnogramStart: Double? { night["hypnogram_start"].d }
  var spo2: Double? { night["spo2"]["median_pct"].d }
  var spo2Min: Double? { night["spo2"]["min_window_pct"].d }
  var skinDelta: Double? { night["skin_temp_delta_c"].d }
  var skin: Double? { night["skin_temp_c"].d }
  var perfusionPct: Double? { night["perfusion"]["vs_previous_nights_pct"].d }
  var respDisturbance: Double? { night["respiratory_disturbance"]["index_0_100"].d }
  var hrSurgesPerH: Double? { night["respiratory_disturbance"]["hr_surges_per_h"].d }
  var postureChangesPerH: Double? { night["movement"]["posture_changes_per_h"].d }
  var regularity: Double? { root["circadian"]["sleep_regularity_index"].d }

  /// Sleep performance vs need (8 h unless the smart-wake plan knows better).
  func sleepScore(needMin: Double) -> Double? {
    guard let a = asleepMin, needMin > 0 else { return nil }
    return min(100, (a / needMin * 100).rounded())
  }
  func sleepWord(_ score: Double?) -> String {
    guard let s = score else { return "En attente" }
    return s >= 85 ? "Réparateur" : (s >= 70 ? "Correct" : "Insuffisant")
  }

  // MARK: effort / day

  var strain: Double? { day["strain_0_21"].d }
  var steps: Int? { day["steps"].i }
  var activeKcal: Double? { day["energy"]["active_kcal"].d }
  var totalKcal: Double? { day["energy"]["total_kcal"].d }
  var hrMean: Double? { day["hr_mean"].d }
  var hrMax: Double? { day["hr_max"].d }
  var mvpa: Double? { day["mvpa_min"].d }
  var walkMin: Double? { day["walk_min"].d }
  var runMin: Double? { day["run_min"].d }
  var cadence: Double? { day["cadence"]["median_spm"].d }
  var sedentaryMin: Double? { day["sedentary"]["sedentary_in_bouts_min"].d }
  var longestSedentary: Double? { day["sedentary"]["longest_bout_min"].d }
  var stressMean: Double? { day["stress"]["mean"].d }
  var stressMax: Double? { day["stress"]["max"].d }
  var stressMinutes: [String: Double] { day["stress"]["minutes"].dict.compactMapValues { $0.d } }
  var coverageH: Double? { day["coverage_h"].d }
  var zones: [Double] { (1...5).map { day["hr_zone_min"]["z\($0)"].d ?? 0 } }
  var hourly: [(hour: Int, hr: Double, steps: Int)] {
    day["hourly"].dict.compactMap { k, v in Int(k).map { ($0, v["hr"].d ?? 0, v["steps"].i ?? 0) } }.sorted { $0.hour < $1.hour }
  }
  var strainWord: String {
    guard let s = strain else { return "En attente" }
    return s >= 18 ? "Très élevé" : (s >= 14 ? "Élevé" : (s >= 10 ? "Modéré" : "Léger"))
  }

  // MARK: load

  var acute7: Double? { root["load"]["acute_7d"].d }
  var chronicWeekly: Double? { root["load"]["chronic_28d_weekly"].d }
  var acwr: Double? { root["load"]["acwr"].d }

  // MARK: signals

  struct Signal { let label: String; let rule: String; let value: String; let flagged: Bool }
  var warningLevel: String { indices["early_warning"]["level"].s ?? "calibrating" }
  var warningNightsNeeded: Int? { indices["early_warning"]["nights_needed"].i }
  var flagged: Set<String> { Set(indices["early_warning"]["signals"].arr.compactMap { $0["signal"].s }) }
  var signals: [Signal] {
    let f = flagged
    return [
      Signal(label: "Température cutanée", rule: "Alerte à partir de +0,5 °C", value: skinDelta.map { "\(FF.signed($0, 1)) °C" } ?? "—", flagged: f.contains("skin_temp_up")),
      Signal(label: "Pouls au repos", rule: "Alerte à partir de +3 bpm", value: rhrDelta.map { "\(FF.signed($0, 0)) bpm" } ?? "—", flagged: f.contains("resting_hr_up")),
      Signal(label: "VFC (variabilité cardiaque)", rule: "Alerte sous −1,5 écart-type", value: night["baselines"]["hrv_rmssd"]["z"].d.map { "\(FF.signed($0, 1)) σ" } ?? "—", flagged: f.contains("hrv_down")),
      Signal(label: "Respiration", rule: "Alerte à partir de +1 /min", value: respDelta.map { "\(FF.signed($0, 1)) /min" } ?? "—", flagged: f.contains("respiration_up")),
      Signal(label: "SpO₂ (saturation en oxygène)", rule: "Alerte à partir de −2 %", value: spo2.map { "\(FF.num($0, 0)) %" } ?? "—", flagged: f.contains("spo2_down")),
    ]
  }
  var signalsTitle: String {
    if !hasNight { return "En attente de la nuit" }
    switch warningLevel {
    case "alert": return "Alerte : plusieurs signaux décalés"
    case "watch": return "À surveiller"
    case "none": return "Rien à signaler"
    default: return "Calibration en cours"
    }
  }
  var signalsSubtitle: String {
    if !hasNight { return "Peau, pouls, VFC, respiration, SpO₂ comparés après ta prochaine nuit" }
    switch warningLevel {
    case "calibrating": return "Encore \(warningNightsNeeded ?? 1) nuit\((warningNightsNeeded ?? 1) > 1 ? "s" : "") pour comparer à ta référence"
    default:
      let ok = signals.filter { !$0.flagged }.count
      return "Peau, pouls, VFC, respiration, SpO₂ : \(ok) sur 5 normaux"
    }
  }

  // MARK: advice

  var advice: (title: String, sub: String, icon: String) {
    if warningLevel == "alert" { return ("Lève le pied aujourd'hui", "Plusieurs signaux de santé sont décalés", "cross.case") }
    if let f = fatigue, f >= 65 { return ("Journée de récupération conseillée", "Fatigue élevée sur 7 jours", "bed.double") }
    switch recoveryZone {
    case .optimal: return ("Bonne fenêtre pour un effort soutenu", fatigue.map { $0 < 40 ? "Fatigue basse · aucun signal d'alerte" : "Récupération optimale" } ?? "Récupération optimale", "bolt")
    case .moderate: return ("Effort modéré recommandé", "Récupération correcte", "figure.walk")
    case .low: return ("Priorise la récupération", "Récupération basse cette nuit", "leaf")
    case .calibrating: return ("Continue de porter le bracelet", "Les conseils arrivent avec ta référence", "sparkles")
    }
  }

  // MARK: history

  func past(_ n: Int) -> [FormeDay] {
    let lo = max(0, index - n + 1)
    guard let raw = root.v as? [String: Any] else { return [] }
    return (lo...index).compactMap { FormeDay(raw: raw, key: all[$0]["date"].s) }
  }
}

extension Forme {
  static func recoveryColor(_ s: Double?) -> Color {
    guard let s else { return Forme.dotOff }
    return s >= 67 ? Forme.green : (s >= 34 ? Forme.blue : Forme.muted)
  }
}

/// "Nuit du 12" — a day's night started the evening before.
func nightChip(_ day: FormeDay) -> String {
  let prev = Calendar.current.date(byAdding: .day, value: -1, to: day.date) ?? day.date
  return "Nuit du \(FF.day(prev, "d"))"
}

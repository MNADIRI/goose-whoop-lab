import Foundation
import HealthKit

/// Body composition (Rust/core/src/body_comp.rs, docs/BODY_COMPOSITION.md): InBody CSV and HealthKit
/// imports into the local database, and the longitudinal summary joined with the strap data.
/// Importing a measurement also re-runs the daily analysis, whose resting energy then uses the
/// measured fat-free mass from that day on.
@MainActor
final class BodyCompStore: ObservableObject {
  static let shared = BodyCompStore()

  @Published private(set) var summary: [String: Any] = [:]
  @Published private(set) var status: String?
  @Published private(set) var busy = false
  /// called after an import changed the data (re-runs the strap analysis for the energy model)
  var onMeasurementsChanged: (() -> Void)?

  private let queue = DispatchQueue(label: "body_comp", qos: .utility)

  var count: Int { summary["count"] as? Int ?? 0 }
  var latest: [String: Any]? { summary["latest"] as? [String: Any] }

  static func person() -> [String: Any] {
    let profile = OnboardingProfileSnapshot()
    var p: [String: Any] = [:]
    if profile.heightMm > 0 { p["height_cm"] = Double(profile.heightMm) / 10 }
    if let age = HealthDataStore.profileAgeYears(from: profile.dateOfBirthString, calendar: .current) { p["age"] = age }
    if let sex = HealthDataStore.normalizedProfileSex(profile.genderRaw) { p["sex"] = sex }
    return p
  }

  func refresh() {
    let args: [String: Any] = ["database_path": HealthDataStore.defaultDatabasePath(), "person": Self.person(), "utc_offset_seconds": TimeZone.current.secondsFromGMT()]
    queue.async { [weak self] in
      let r = try? GooseRustBridge().request(method: "body_comp.summary", args: args)
      Task { @MainActor in
        guard let self, let r else { return }
        self.summary = r
        self.syncProfileWeight()
      }
    }
  }

  private static let profileWeightTsKey = "bodycomp.profileWeightTs"

  /// The profile weight (Profil, energy before the first measurement, onboarding) follows the
  /// latest weighing: each measurement newer than the last one applied replaces it once, so a later
  /// manual edit is kept until the next weighing.
  private func syncProfileWeight() {
    let l = BCVal(latest)
    guard let ts = l.num("ts"), let kg = l.num("values.weight_kg"), kg > 20, kg < 400 else { return }
    let applied = UserDefaults.standard.double(forKey: Self.profileWeightTsKey)
    guard ts > applied else { return }
    var profile = OnboardingProfileSnapshot()
    let grams = Int((kg * 1000).rounded())
    UserDefaults.standard.set(ts, forKey: Self.profileWeightTsKey)
    guard profile.weightGrams != grams else { return }
    profile.weightGrams = grams
    let shown = profile.unitSystemRaw == "imperial" ? kg / 0.45359237 : kg
    profile.weightInput = String(format: "%.1f", shown)
    let complete = UserDefaults.standard.bool(forKey: OnboardingStorage.onboardingComplete)
    if profile.hasRequiredDetails {
      OnboardingProfilePersistence.saveProfile(profile, onboardingComplete: complete)
    } else {
      profile.write()
    }
  }

  func importCSV(url: URL) {
    let scoped = url.startAccessingSecurityScopedResource()
    defer { if scoped { url.stopAccessingSecurityScopedResource() } }
    guard let data = try? Data(contentsOf: url), let text = String(data: data, encoding: .utf8) ?? String(data: data, encoding: .isoLatin1) else {
      status = "Fichier illisible"
      return
    }
    busy = true
    // the measurement's own local time zone is unknown: the current offset is used
    let args: [String: Any] = ["database_path": HealthDataStore.defaultDatabasePath(), "csv": text, "file_name": url.lastPathComponent, "utc_offset_seconds": TimeZone.current.secondsFromGMT()]
    queue.async { [weak self] in
      let result = Result { try GooseRustBridge().request(method: "body_comp.import_csv", args: args) }
      Task { @MainActor in self?.finishImport(result, kind: "CSV") }
    }
  }

  private func finishImport(_ result: Result<[String: Any], Error>, kind: String) {
    busy = false
    switch result {
    case .success(let r):
      let added = r["added"] as? Int ?? 0, updated = r["updated"] as? Int ?? 0, same = (r["unchanged"] as? Int ?? 0) + (r["already_known"] as? Int ?? 0)
      let dup = r["skipped_same_weighing_as_csv"] as? Int ?? 0
      var parts: [String] = []
      if added > 0 { parts.append("\(added) ajoutée\(added > 1 ? "s" : "")") }
      if updated > 0 { parts.append("\(updated) corrigée\(updated > 1 ? "s" : "")") }
      if same > 0 { parts.append("\(same) déjà connue\(same > 1 ? "s" : "")") }
      if dup > 0 { parts.append("\(dup) doublon\(dup > 1 ? "s" : "") de l'InBody ignoré\(dup > 1 ? "s" : "")") }
      status = parts.isEmpty ? "Aucune mesure dans le fichier" : "\(kind) : " + parts.joined(separator: " · ")
      if added + updated > 0 { onMeasurementsChanged?() }
    case .failure(let e):
      status = "Import impossible : \(e.localizedDescription)"
    }
    refresh()
  }

  func setExcluded(_ id: String, _ excluded: Bool) {
    call("body_comp.set_excluded", ["id": id, "excluded": excluded])
  }

  func delete(_ id: String) {
    call("body_comp.delete", ["id": id])
  }

  private func call(_ method: String, _ extra: [String: Any]) {
    var args = extra
    args["database_path"] = HealthDataStore.defaultDatabasePath()
    queue.async { [weak self] in
      _ = try? GooseRustBridge().request(method: method, args: args)
      Task { @MainActor in
        self?.refresh()
        self?.onMeasurementsChanged?()
      }
    }
  }

  // MARK: HealthKit

  private static let hkTypes: [(HKQuantityTypeIdentifier, String, HKUnit)] = [
    (.bodyMass, "weight_kg", .gramUnit(with: .kilo)),
    (.bodyFatPercentage, "pbf_pct", .percent()),
    (.leanBodyMass, "lean_mass_kg", .gramUnit(with: .kilo)),
  ]

  /// Weight, body fat and lean body mass from Santé (last 2 years). Samples written by this app are
  /// skipped; samples of an InBody weighing already imported by CSV are recognised in Rust.
  func importHealthKit() async {
    guard HKHealthStore.isHealthDataAvailable() else { status = "Santé indisponible sur cet appareil"; return }
    let store = HKHealthStore()
    let types = Set(Self.hkTypes.compactMap { HKObjectType.quantityType(forIdentifier: $0.0) })
    do {
      try await store.requestAuthorization(toShare: [], read: types)
    } catch {
      status = "Accès à Santé refusé"
      return
    }
    busy = true
    var samples: [[String: Any]] = []
    let since = Date().addingTimeInterval(-730 * 86_400)
    let own = Bundle.main.bundleIdentifier ?? ""
    for (id, kind, unit) in Self.hkTypes {
      guard let type = HKQuantityType.quantityType(forIdentifier: id) else { continue }
      let found: [HKQuantitySample] = await withCheckedContinuation { cont in
        let q = HKSampleQuery(sampleType: type, predicate: HKQuery.predicateForSamples(withStart: since, end: nil), limit: HKObjectQueryNoLimit, sortDescriptors: nil) { _, s, _ in
          cont.resume(returning: (s as? [HKQuantitySample]) ?? [])
        }
        store.execute(q)
      }
      for s in found where s.sourceRevision.source.bundleIdentifier != own {
        var v = s.quantity.doubleValue(for: unit)
        if kind == "pbf_pct" { v *= 100 }
        samples.append(["uuid": s.uuid.uuidString, "ts": Int(s.startDate.timeIntervalSince1970), "kind": kind, "value": v,
                        "source_name": s.sourceRevision.source.name, "source_bundle": s.sourceRevision.source.bundleIdentifier])
      }
    }
    let args: [String: Any] = ["database_path": HealthDataStore.defaultDatabasePath(), "samples": samples]
    let result: Result<[String: Any], Error> = await withCheckedContinuation { cont in
      queue.async { cont.resume(returning: Result { try GooseRustBridge().request(method: "body_comp.import_healthkit", args: args) }) }
    }
    finishImport(result, kind: "Santé")
  }

  // MARK: Coach

  /// Compact view for the coach: latest values, change with its error margin, trend classes.
  var coachPayload: [String: Any] {
    guard count > 0 else { return ["available": false, "how_to_add": "Indicateurs > Composition corporelle > Importer un CSV InBody"] }
    var p: [String: Any] = ["available": true, "measurements": count, "algorithm_version": summary["algorithm_version"] ?? NSNull()]
    if let l = latest {
      p["latest"] = ["time": l["local_time"] ?? NSNull(), "device": l["device"] ?? NSNull(), "values": l["values"] ?? NSNull(), "derived": l["derived"] ?? NSNull()]
    }
    p["change_vs_previous"] = summary["change"] ?? NSNull()
    p["recomposition"] = summary["recomposition"] ?? NSNull()
    p["segmental"] = summary["segmental"] ?? NSNull()
    p["metabolism"] = summary["metabolism"] ?? NSNull()
    if let a = summary["adaptation"] as? [String: Any] {
      p["adaptation"] = ["horizons": a["horizons"] ?? NSNull(), "training_consistency_28d": a["training_consistency_28d"] ?? NSNull(), "resistance_volume": a["resistance_volume"] ?? NSNull(), "associations_note": a["associations_note"] ?? NSNull()]
    }
    p["caveats"] = "BIA estimates; a single pair of measurements is not read as tissue change below the MDC95; not a medical or cardiovascular risk assessment."
    return p
  }
}

/// Read helpers over the summary JSON.
struct BCVal {
  let d: [String: Any]
  init(_ d: [String: Any]?) { self.d = d ?? [:] }
  subscript(_ path: String) -> Any? {
    var cur: Any? = d
    for k in path.split(separator: ".") { cur = (cur as? [String: Any])?[String(k)] }
    return cur is NSNull ? nil : cur
  }
  func num(_ path: String) -> Double? { (self[path] as? Double) ?? (self[path] as? Int).map(Double.init) }
  func str(_ path: String) -> String? { self[path] as? String }
  func obj(_ path: String) -> BCVal { BCVal(self[path] as? [String: Any]) }
  func series(_ path: String) -> [(Date, Double)] {
    (self[path] as? [[Any]] ?? []).compactMap { p in
      guard p.count == 2, let t = (p[0] as? Int).map(Double.init) ?? (p[0] as? Double), let v = (p[1] as? Double) ?? (p[1] as? Int).map(Double.init) else { return nil }
      return (Date(timeIntervalSince1970: t), v)
    }
  }
}

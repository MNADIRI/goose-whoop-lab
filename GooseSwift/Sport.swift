import Charts
import Foundation
import SwiftUI

// MARK: - Model

struct SportSegment: Identifiable, Hashable {
  let id: String
  let kind: String
  let start: Date
  let end: Date
  let repsPred: Int?
  let repsLabel: Int?
  let exercisePred: String?
  let exerciseConf: Double?
  let exerciseLabel: String?
  let confidence: Double?
  let labelStatus: String
  let periodS: Double?
  let upS: Double?
  let downS: Double?
  let holdS: Double?
  let concentric: String
  let strokes: Int?
  let cluster: Int?

  init(_ d: [String: Any]) {
    id = d["id"] as? String ?? UUID().uuidString
    kind = d["kind"] as? String ?? "set"
    start = Date(timeIntervalSince1970: d["start_ts"] as? Double ?? 0)
    end = Date(timeIntervalSince1970: d["end_ts"] as? Double ?? 0)
    repsPred = d["reps_pred"] as? Int
    repsLabel = d["reps_label"] as? Int
    exercisePred = d["exercise_pred"] as? String
    exerciseConf = d["exercise_conf"] as? Double
    exerciseLabel = d["exercise_label"] as? String
    confidence = d["confidence"] as? Double
    labelStatus = d["label_status"] as? String ?? "pending"
    let detail = d["detail"] as? [String: Any] ?? [:]
    periodS = detail["period_s"] as? Double
    upS = detail["up_s"] as? Double
    downS = detail["down_s"] as? Double
    holdS = detail["hold_s"] as? Double
    concentric = detail["concentric"] as? String ?? "up"
    strokes = detail["strokes"] as? Int
    cluster = detail["cluster"] as? Int
  }

  var reps: Int? { repsLabel ?? repsPred }
  var exercise: String? { exerciseLabel ?? exercisePred }
  var isValidated: Bool { labelStatus == "confirmed" || labelStatus == "corrected" }
}

struct SportSession: Identifiable, Hashable {
  let id: String
  let start: Date
  let end: Date?
  let status: String
  let source: String
  let predicted: String?
  let confidence: Double?
  let probs: [String: Double]
  let label: String?
  let labelStatus: String
  let modelVersion: String?
  let durationMin: Double?
  let hrMean: Double?
  let hrMax: Double?
  let strokes: Int?
  let sets: Int?
  let reps: Int?
  let cadence: Double?
  let imuMinutes: Double?
  let hrSeries: [(Date, Double)]
  let segments: [SportSegment]

  init(_ d: [String: Any]) {
    id = d["id"] as? String ?? ""
    start = Date(timeIntervalSince1970: TimeInterval(d["start_ts"] as? Int ?? 0))
    end = (d["end_ts"] as? Int).map { Date(timeIntervalSince1970: TimeInterval($0)) }
    status = d["status"] as? String ?? ""
    source = d["source"] as? String ?? "auto"
    predicted = d["predicted_type"] as? String
    confidence = d["confidence"] as? Double
    probs = (d["probs"] as? [String: Any] ?? [:]).compactMapValues { $0 as? Double }
    label = d["label_type"] as? String
    labelStatus = d["label_status"] as? String ?? "pending"
    modelVersion = d["model_version"] as? String
    let s = d["summary"] as? [String: Any] ?? [:]
    durationMin = s["duration_min"] as? Double
    hrMean = s["hr_mean"] as? Double
    hrMax = s["hr_max"] as? Double
    strokes = s["strokes"] as? Int
    sets = s["sets"] as? Int
    reps = s["reps"] as? Int
    cadence = s["cadence_spm"] as? Double
    imuMinutes = s["imu_minutes"] as? Double
    hrSeries = (s["hr_series"] as? [[Any]] ?? []).compactMap { p in
      guard p.count == 2, let t = (p[0] as? Int).map(Double.init) ?? (p[0] as? Double), let v = p[1] as? Double else { return nil }
      return (Date(timeIntervalSince1970: t), v)
    }
    segments = (d["segments"] as? [[String: Any]] ?? []).map(SportSegment.init)
  }

  static func == (a: SportSession, b: SportSession) -> Bool { a.id == b.id && a.labelStatus == b.labelStatus && a.label == b.label && a.segments == b.segments }
  func hash(into h: inout Hasher) { h.combine(id) }

  var type: String { label ?? predicted ?? "autre" }

  /// Valid strength sets grouped by exercise (named, else the engine's movement group), in order.
  var exerciseGroups: [(name: String?, key: String, sets: [SportSegment])] {
    var out: [(name: String?, key: String, sets: [SportSegment])] = []
    for s in segments where s.kind == "set" && s.labelStatus != "rejected" {
      let key = s.exercise ?? "groupe-\(s.cluster ?? -1)"
      if let i = out.firstIndex(where: { $0.key == key }) { out[i].sets.append(s) } else { out.append((s.exercise, key, [s])) }
    }
    return out
  }

  /// "3 exercices · 9 séries · 84 rép." for strength sessions with sets.
  var strengthLine: String? {
    let g = exerciseGroups
    guard SportKind.of(type) == .musculation, !g.isEmpty else { return nil }
    let sets = g.reduce(0) { $0 + $1.sets.count }
    let reps = g.reduce(0) { $0 + $1.sets.reduce(0) { $0 + ($1.reps ?? 0) } }
    return "\(g.count) exercice\(g.count > 1 ? "s" : "") · \(sets) série\(sets > 1 ? "s" : "") · \(reps) rép."
  }
  var isValidated: Bool { ["confirmed", "corrected", "manual", "rejected"].contains(labelStatus) }
}

enum SportKind: String, CaseIterable, Identifiable {
  case course, tennis, musculation, autre
  var id: String { rawValue }
  var title: String {
    switch self {
    case .course: "Course"
    case .tennis: "Tennis"
    case .musculation: "Musculation"
    case .autre: "Autre"
    }
  }
  var icon: String {
    switch self {
    case .course: "figure.run"
    case .tennis: "figure.tennis"
    case .musculation: "dumbbell.fill"
    case .autre: "figure.mixed.cardio"
    }
  }
  var tint: Color {
    switch self {
    case .course: .orange
    case .tennis: .green
    case .musculation: .purple
    case .autre: .blue
    }
  }
  static func of(_ s: String?) -> SportKind { SportKind(rawValue: s ?? "") ?? .autre }
}

// MARK: - Controller

/// Runs the Rust sport engine (`sport.update`, Rust/core/src/sport.rs) every 30 s and after each
/// history sync; switches the live HR + IMU streams on while a session is plausible or running
/// (watch / active / ending, which includes the post-activity buffer) and off afterwards. The
/// pre-activity buffer comes from R18 (always recorded on the strap). Errors are retried silently.
@MainActor
final class SportController: ObservableObject {
  @Published private(set) var state = "idle"
  @Published private(set) var current: [String: Any] = [:]
  @Published private(set) var sessions: [SportSession] = []
  @Published private(set) var reviewQueue: [String] = []
  @Published private(set) var exercises: [String] = []
  @Published private(set) var dataset: [String: Int] = [:]
  @Published private(set) var capturing = false
  @Published private(set) var liveHR: Int?
  @Published private(set) var lagSeconds: Int?
  @Published var enabled: Bool = UserDefaults.standard.object(forKey: "sport.enabled") as? Bool ?? true {
    didSet { UserDefaults.standard.set(enabled, forKey: "sport.enabled") }
  }

  private weak var ble: GooseBLEClient?
  private weak var smartWake: SmartWakeController?
  private weak var export: WhoopLabAutoExport?
  private let queue = DispatchQueue(label: "sport.engine", qos: .utility)
  nonisolated(unsafe) private var pendingLive: [Int: (hr: Double?, rr: [Double])] = [:]
  nonisolated(unsafe) private var pendingIMU: [(Double, String)] = []
  nonisolated private let lock = NSLock()
  private var timer: Timer?
  private var running = false
  private var lastSync = Date.distantPast
  private var syncInterval: TimeInterval = 300
  private var streamsOwned = false

  func attach(ble: GooseBLEClient, smartWake: SmartWakeController, export: WhoopLabAutoExport) {
    self.ble = ble
    self.smartWake = smartWake
    self.export = export
    ble.sportLiveSampleSink = { [weak self] date, hr, rr, _ in self?.collect(date: date, hr: hr, rr: rr) }
    ble.sportIMUSink = { [weak self] hex, date in self?.collectIMU(hex: hex, at: date) }
    timer = Timer.scheduledTimer(withTimeInterval: 30, repeats: true) { [weak self] _ in Task { @MainActor in self?.tick() } }
    refresh()
    DispatchQueue.main.asyncAfter(deadline: .now() + 15) { [weak self] in self?.backfill() }
  }

  /// Retrospective detection on the history the phone already holds (incremental, resumes where
  /// the last run stopped). Labels the user gave outside the app are applied once.
  private static let seedKey = "sport.seed.v1"
  private func backfill() {
    let seeded = UserDefaults.standard.bool(forKey: Self.seedKey)
    // 11 Nov 2026 tennis match (confirmed by the user)
    let labels: [[String: Any]] = seeded ? [] : [["ts": 1_794_398_400, "type": "tennis"]]
    call("sport.backfill", ["days": 14, "labels": labels]) { [weak self] r in
      if !seeded, r["labelled"] as? Int ?? 0 > 0 { UserDefaults.standard.set(true, forKey: Self.seedKey) }
      self?.refresh()
      if (r["sessions"] as? [Any])?.isEmpty == false { self?.exportDataset() }
    }
  }

  nonisolated private func collect(date: Date, hr: Int?, rr: [Double]) {
    let ts = Int(date.timeIntervalSince1970)
    lock.lock()
    var e = pendingLive[ts] ?? (nil, [])
    if let hr { e.hr = Double(hr) }
    e.rr += rr
    pendingLive[ts] = e
    lock.unlock()
  }

  nonisolated private func collectIMU(hex: String, at date: Date) {
    lock.lock()
    pendingIMU.append((date.timeIntervalSince1970, hex))
    lock.unlock()
  }

  func historicalSyncProgressed() {
    guard ble?.isHistoricalSyncing == false else { return }
    tick(force: true)
  }

  private var paused: Bool { !enabled || smartWake?.session != nil }

  private func tick(force: Bool = false) {
    guard let ble, ble.connectionState == "ready" || force else { return }
    if capturing || state != "idle", ble.canSyncHistorical, Date().timeIntervalSince(lastSync) >= syncInterval {
      lastSync = Date()
      ble.syncHistoricalPackets()
    }
    update()
  }

  private func update() {
    guard !running else { return }
    running = true
    lock.lock()
    let live = pendingLive.sorted { $0.key < $1.key }.map { [$0.key, $0.value.hr ?? 0, $0.value.rr] as [Any] }
    pendingLive.removeAll()
    let imu = pendingIMU.map { [$0.0, $0.1] as [Any] }
    pendingIMU.removeAll()
    lock.unlock()
    let args: [String: Any] = ["database_path": HealthDataStore.defaultDatabasePath(), "live": live, "imu": imu, "capturing": capturing, "paused": paused]
    queue.async { [weak self] in
      let r = try? GooseRustBridge().request(method: "sport.update", args: args)
      Task { @MainActor in self?.apply(r, retry: (live, imu)) }
    }
  }

  private func apply(_ r: [String: Any]?, retry: ([[Any]], [[Any]])) {
    running = false
    guard let r else {
      // engine busy (database locked): keep the samples for the next call, no message
      lock.lock()
      for row in retry.0 {
        if let ts = row[0] as? Int { pendingLive[ts] = (row[1] as? Double, row[2] as? [Double] ?? []) }
      }
      pendingIMU.insert(contentsOf: retry.1.compactMap { row in (row[0] as? Double).flatMap { a in (row[1] as? String).map { (a, $0) } } }, at: 0)
      lock.unlock()
      return
    }
    state = r["state"] as? String ?? state
    current = r["current"] as? [String: Any] ?? [:]
    lagSeconds = r["lag_s"] as? Int
    syncInterval = TimeInterval(r["sync_interval_s"] as? Int ?? 300)
    liveHR = ble?.liveHeartRateBPM
    let want = r["want_streams"] as? Bool ?? false
    if want && !capturing { setStreams(true) }
    if !want && capturing { setStreams(false) }
    if let event = r["event"] as? String, event == "end" || event == "start" {
      refresh()
      if event == "end" { exportDataset() }
    }
  }

  private func setStreams(_ on: Bool) {
    guard let ble else { return }
    // the smart wake owns the streams while it is armed
    if !on, smartWake?.session != nil { capturing = false; streamsOwned = false; return }
    typealias K = GooseBLEClient.SensorStreamCommandKind
    let cmds: [K] = on
      ? [K(commandNumber: 3, payload: [1], name: "TOGGLE_REALTIME_HR_ON"), K(commandNumber: 106, payload: K.revisionBoolean(true), name: "TOGGLE_IMU_MODE_ON")]
      : [K(commandNumber: 106, payload: K.revisionBoolean(false), name: "TOGGLE_IMU_MODE_OFF"), K(commandNumber: 3, payload: [0], name: "TOGGLE_REALTIME_HR_OFF")]
    capturing = on
    streamsOwned = on
    ble.whenFreeForCommands(timeout: 120) { $0.writeSensorStreamCommands(cmds, requestedStatus: "Sport", updatePhysiologyStatus: false) }
    ble.record(source: "sport", title: on ? "streams.on" : "streams.off", body: "state=\(state)")
  }

  // MARK: user actions

  func startManual(_ kind: SportKind) {
    call("sport.manual", ["action": "start", "sport": kind.rawValue]) { [weak self] r in
      self?.state = r["state"] as? String ?? "active"
      if !(self?.capturing ?? true) { self?.setStreams(true) }
      self?.update()
      self?.refresh()
    }
  }

  func stopManual() {
    call("sport.manual", ["action": "stop"]) { [weak self] _ in
      self?.state = "idle"
      self?.current = [:]
      // keep the post-activity buffer: streams off 3 minutes later
      DispatchQueue.main.asyncAfter(deadline: .now() + 180) { [weak self] in
        if self?.state == "idle", self?.capturing == true { self?.setStreams(false) }
      }
      self?.refresh()
      self?.exportDataset()
    }
  }

  /// Permanently removes a session started by mistake (its deletion stays in the annotation log).
  func delete(_ id: String) {
    call("sport.delete", ["id": id]) { [weak self] r in
      guard let self else { return }
      if r["stopped"] as? Bool == true {
        self.state = "idle"
        self.current = [:]
        if self.capturing { self.setStreams(false) }
      }
      self.sessions.removeAll { $0.id == id }
      self.reviewQueue.removeAll { $0 == id }
      self.refresh()
      self.exportDataset()
    }
  }

  func annotate(_ target: String, _ id: String, _ field: String, _ value: String? = nil) {
    var args: [String: Any] = ["target": target, "id": id, "field": field]
    if let value { args["value"] = value }
    call("sport.annotate", args) { [weak self] _ in
      self?.refresh()
      self?.scheduleExport()
    }
  }

  func refresh() {
    call("sport.list", ["days": 60]) { [weak self] r in
      guard let self else { return }
      self.sessions = (r["sessions"] as? [[String: Any]] ?? []).map(SportSession.init)
      self.reviewQueue = r["review_queue"] as? [String] ?? []
      self.exercises = r["exercises"] as? [String] ?? []
      self.dataset = (r["dataset"] as? [String: Any] ?? [:]).compactMapValues { $0 as? Int }
      if (r["reanalyzed"] as? Int ?? 0) > 0 { self.scheduleExport() }
    }
  }

  private var exportWork: DispatchWorkItem?
  /// Several answers in a row (a whole exercise group) give a single export.
  private func scheduleExport() {
    exportWork?.cancel()
    let w = DispatchWorkItem { [weak self] in self?.exportDataset() }
    exportWork = w
    DispatchQueue.main.asyncAfter(deadline: .now() + 5, execute: w)
  }

  /// Dataset to the whoop-lab folder (iCloud) under sport/, else to the app's Documents.
  func exportDataset() {
    var dir = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0].appendingPathComponent("sport_dataset", isDirectory: true)
    var scoped: URL?
    if let folder = export?.resolveFolder(), folder.startAccessingSecurityScopedResource() {
      scoped = folder
      dir = folder.appendingPathComponent("sport", isDirectory: true)
    }
    // a personal model trained offline (lab/sport/train.py) is picked up from <dataset>/models
    let models = dir.appendingPathComponent("models", isDirectory: true)
    let newest = (try? FileManager.default.contentsOfDirectory(at: models, includingPropertiesForKeys: nil))?
      .filter { $0.pathExtension == "json" }.sorted { $0.lastPathComponent < $1.lastPathComponent }.last
    var install: [String: Any]?
    if let newest, let data = try? Data(contentsOf: newest), let m = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
       let version = m["version"] as? String, version != UserDefaults.standard.string(forKey: "sport.installedModel") {
      install = ["version": version, "kind": m["kind"] as? String ?? "logreg", "params": m["params"] ?? [:], "metrics": m["metrics"] ?? [:]]
    }
    call("sport.export_dataset", ["output_dir": dir.path]) { _ in scoped?.stopAccessingSecurityScopedResource() }
    if let install {
      call("sport.install_model", install) { _ in UserDefaults.standard.set(install["version"], forKey: "sport.installedModel") }
    }
  }

  private func call(_ method: String, _ extra: [String: Any], _ done: @escaping @MainActor ([String: Any]) -> Void) {
    var args = extra
    args["database_path"] = HealthDataStore.defaultDatabasePath()
    queue.async {
      let r = try? GooseRustBridge().request(method: method, args: args)
      Task { @MainActor in
        if let r { done(r) }
      }
    }
  }
}

// MARK: - Views

struct SportView: View {
  @ObservedObject var controller: SportController
  @State private var tick = Date()
  @State private var pendingDelete: String?
  private let clock = Timer.publish(every: 1, on: .main, in: .common).autoconnect()

  private var queue: [SportSession] { controller.reviewQueue.compactMap { id in controller.sessions.first { $0.id == id } } }
  private var history: [SportSession] { controller.sessions.filter { !controller.reviewQueue.contains($0.id) && $0.status == "ended" } }

  var body: some View {
    ScrollView {
      VStack(spacing: 18) {
        liveCard
        if !queue.isEmpty {
          sectionTitle("À vérifier", count: queue.count)
          ForEach(queue) { s in
            ReviewCard(session: s, controller: controller)
              .contextMenu { Button("Supprimer la séance", systemImage: "trash", role: .destructive) { pendingDelete = s.id } }
          }
        }
        if !history.isEmpty {
          sectionTitle("Séances", count: nil)
          ForEach(history) { s in
            NavigationLink(value: s) { SessionRow(session: s) }.buttonStyle(.plain)
              .contextMenu { Button("Supprimer la séance", systemImage: "trash", role: .destructive) { pendingDelete = s.id } }
          }
        }
        datasetCard
      }
      .padding(.horizontal, 16)
      .padding(.vertical, 12)
      .frame(maxWidth: 640)
      .frame(maxWidth: .infinity)
    }
    .gooseScreenBackground()
    .navigationTitle("Sport")
    .navigationDestination(for: SportSession.self) { s in
      SessionDetailView(sessionID: s.id, controller: controller)
    }
    .onAppear { controller.refresh() }
    .onReceive(clock) { tick = $0 }
    .confirmationDialog("Supprimer cette séance ?", isPresented: Binding(get: { pendingDelete != nil }, set: { if !$0 { pendingDelete = nil } }), titleVisibility: .visible) {
      Button("Supprimer", role: .destructive) { if let id = pendingDelete { controller.delete(id) }; pendingDelete = nil }
      Button("Annuler", role: .cancel) { pendingDelete = nil }
    } message: {
      Text("Elle disparaît de l'historique et du jeu de données. Cette action est définitive.")
    }
    .refreshable { controller.refresh() }
  }

  private func sectionTitle(_ t: String, count: Int?) -> some View {
    HStack {
      Text(t).font(.title3.weight(.semibold))
      if let count { Text("\(count)").font(.subheadline.weight(.semibold)).padding(.horizontal, 8).padding(.vertical, 2).background(.orange.opacity(0.2), in: Capsule()) }
      Spacer()
    }
  }

  @ViewBuilder private var liveCard: some View {
    let active = controller.state == "active" || controller.state == "ending"
    VStack(alignment: .leading, spacing: 14) {
      if active {
        let kind = SportKind.of(controller.current["label_type"] as? String ?? controller.current["guess"] as? String)
        let start = (controller.current["start_ts"] as? Int).map { Date(timeIntervalSince1970: TimeInterval($0)) } ?? tick
        HStack(spacing: 12) {
          Image(systemName: kind.icon).font(.title).foregroundStyle(kind.tint).frame(width: 48, height: 48).background(kind.tint.opacity(0.15), in: Circle())
          VStack(alignment: .leading, spacing: 2) {
            Text(controller.current["label_type"] == nil ? "\(kind.title) probable" : kind.title).font(.title3.weight(.semibold))
            Text(controller.state == "ending" ? "Fin probable…" : "Séance en cours").font(.subheadline).foregroundStyle(.secondary)
          }
          Spacer()
          Text(elapsed(from: start)).font(.system(.title2, design: .rounded).monospacedDigit().weight(.semibold))
        }
        HStack(spacing: 10) {
          metric("FC", controller.liveHR.map { "\($0)" } ?? "–", "bpm")
          if kind == .musculation {
            metric("Séries", "\(controller.current["sets_last10"] as? Int ?? 0)", "10 min")
            metric("Rép.", "\(controller.current["reps_last10"] as? Int ?? 0)", "10 min")
          }
          if kind == .tennis { metric("Frappes", "\(controller.current["strokes_last10"] as? Int ?? 0)", "10 min") }
          metric("Capteurs", controller.capturing ? "100 Hz" : "1 Hz", controller.capturing ? "IMU" : "historique")
        }
        HStack(spacing: 10) {
          Button(role: .destructive) { controller.stopManual() } label: {
            Label("Terminer la séance", systemImage: "stop.fill").frame(maxWidth: .infinity)
          }.buttonStyle(.bordered).controlSize(.large)
          if let id = controller.current["id"] as? String {
            Button { pendingDelete = id } label: { Label("Annuler", systemImage: "trash") }.buttonStyle(.bordered).controlSize(.large).tint(.gray)
          }
        }
      } else {
        HStack {
          VStack(alignment: .leading, spacing: 2) {
            Text(controller.state == "watch" ? "Activité possible…" : "Aucune séance en cours").font(.title3.weight(.semibold))
            Text(controller.enabled ? "Détection automatique active" : "Détection automatique en pause").font(.subheadline).foregroundStyle(.secondary)
          }
          Spacer()
          Toggle("", isOn: $controller.enabled).labelsHidden()
        }
        LazyVGrid(columns: [GridItem(.flexible()), GridItem(.flexible())], spacing: 10) {
          ForEach(SportKind.allCases) { k in
            Button { controller.startManual(k) } label: {
              HStack {
                Image(systemName: k.icon)
                Text(k.title).fontWeight(.semibold)
                Spacer()
              }
              .padding(.vertical, 12).padding(.horizontal, 14)
              .foregroundStyle(k.tint)
              .background(k.tint.opacity(0.13), in: RoundedRectangle(cornerRadius: 14, style: .continuous))
            }.buttonStyle(.plain)
          }
        }
      }
    }
    .padding(18)
    .frame(maxWidth: .infinity, alignment: .leading)
    .background(.background.secondary, in: RoundedRectangle(cornerRadius: 22, style: .continuous))
  }

  private var datasetCard: some View {
    VStack(alignment: .leading, spacing: 8) {
      Label("Jeu de données personnel", systemImage: "externaldrive.badge.checkmark").font(.headline)
      HStack(spacing: 10) {
        metric("Séances", "\(controller.dataset["labelled_sessions"] ?? 0)", "validées")
        metric("Séries", "\(controller.dataset["labelled_sets"] ?? 0)", "validées")
        metric("Réponses", "\(controller.dataset["annotations"] ?? 0)", "au total")
      }
      Text("Enregistré automatiquement après chaque séance et chaque validation.").font(.caption).foregroundStyle(.secondary)
    }
    .padding(16)
    .frame(maxWidth: .infinity, alignment: .leading)
    .background(.background.secondary, in: RoundedRectangle(cornerRadius: 22, style: .continuous))
  }

  private func elapsed(from start: Date) -> String {
    let s = max(0, Int(tick.timeIntervalSince(start)))
    return s >= 3600 ? String(format: "%d:%02d:%02d", s / 3600, s / 60 % 60, s % 60) : String(format: "%d:%02d", s / 60, s % 60)
  }
}

private func metric(_ title: String, _ value: String, _ unit: String) -> some View {
  VStack(alignment: .leading, spacing: 2) {
    Text(title).font(.caption).foregroundStyle(.secondary)
    Text(value).font(.system(.title3, design: .rounded).weight(.semibold)).monospacedDigit()
    Text(unit).font(.caption2).foregroundStyle(.secondary)
  }
  .frame(maxWidth: .infinity, alignment: .leading)
  .padding(10)
  .background(.background.tertiary, in: RoundedRectangle(cornerRadius: 12, style: .continuous))
}

private func timeRange(_ s: SportSession) -> String {
  let a = s.start.formatted(date: .abbreviated, time: .shortened)
  guard let e = s.end else { return a }
  return a + " – " + e.formatted(date: .omitted, time: .shortened)
}

/// One-tap validation: the predicted type is the first chip; any other chip corrects it.
struct ReviewCard: View {
  let session: SportSession
  @ObservedObject var controller: SportController

  var body: some View {
    let kind = SportKind.of(session.predicted)
    VStack(alignment: .leading, spacing: 12) {
      NavigationLink(value: session) {
        HStack(spacing: 12) {
          Image(systemName: kind.icon).font(.title2).foregroundStyle(kind.tint).frame(width: 42, height: 42).background(kind.tint.opacity(0.15), in: Circle())
          VStack(alignment: .leading, spacing: 2) {
            Text("\(kind.title) ?").font(.headline)
            Text(timeRange(session)).font(.caption).foregroundStyle(.secondary)
            if let line = session.strengthLine { Text(line).font(.caption.weight(.medium)).foregroundStyle(kind.tint) }
          }
          Spacer()
          VStack(alignment: .trailing, spacing: 2) {
            if let c = session.confidence { Text("\(Int((c * 100).rounded())) %").font(.subheadline.weight(.semibold)).foregroundStyle(c < 0.6 ? .orange : .secondary) }
            if let d = session.durationMin { Text("\(Int(d)) min").font(.caption).foregroundStyle(.secondary) }
          }
          Image(systemName: "chevron.right").font(.caption).foregroundStyle(.tertiary)
        }
      }.buttonStyle(.plain)
      ScrollView(.horizontal, showsIndicators: false) {
        HStack(spacing: 8) {
          chip("Oui, \(kind.title.lowercased())", kind.tint, filled: true) { controller.annotate("session", session.id, "confirm") }
          ForEach(SportKind.allCases.filter { $0 != kind }) { k in
            chip(k.title, k.tint, filled: false) { controller.annotate("session", session.id, "type", k.rawValue) }
          }
          chip("Pas une séance", .gray, filled: false) { controller.annotate("session", session.id, "reject") }
        }
      }
    }
    .padding(16)
    .background(.background.secondary, in: RoundedRectangle(cornerRadius: 20, style: .continuous))
  }

  private func chip(_ t: String, _ c: Color, filled: Bool, _ action: @escaping () -> Void) -> some View {
    Button(action: action) {
      Text(t).font(.subheadline.weight(.semibold)).padding(.horizontal, 14).padding(.vertical, 9)
        .foregroundStyle(filled ? .white : c)
        .background(filled ? AnyShapeStyle(c) : AnyShapeStyle(c.opacity(0.13)), in: Capsule())
    }.buttonStyle(.plain)
  }
}

struct SessionRow: View {
  let session: SportSession
  var body: some View {
    let kind = SportKind.of(session.type)
    HStack(spacing: 12) {
      Image(systemName: session.labelStatus == "rejected" ? "xmark.circle" : kind.icon).foregroundStyle(session.labelStatus == "rejected" ? .gray : kind.tint).frame(width: 36, height: 36).background((session.labelStatus == "rejected" ? Color.gray : kind.tint).opacity(0.13), in: Circle())
      VStack(alignment: .leading, spacing: 2) {
        Text(session.labelStatus == "rejected" ? "Pas une séance" : kind.title).font(.subheadline.weight(.semibold))
        Text(timeRange(session)).font(.caption).foregroundStyle(.secondary)
        if let line = session.strengthLine { Text(line).font(.caption.weight(.medium)).foregroundStyle(kind.tint) }
      }
      Spacer()
      if let d = session.durationMin { Text("\(Int(d)) min").font(.subheadline).foregroundStyle(.secondary) }
      if session.isValidated { Image(systemName: "checkmark.seal.fill").foregroundStyle(.green) }
    }
    .padding(12)
    .background(.background.secondary, in: RoundedRectangle(cornerRadius: 16, style: .continuous))
  }
}

struct SessionDetailView: View {
  let sessionID: String
  @ObservedObject var controller: SportController
  @Environment(\.dismiss) private var dismiss
  @State private var confirmDelete = false
  @State private var newExerciseFor: SportSegment?
  @State private var newExerciseGroup: [SportSegment]?
  @State private var newExercise = ""

  private var session: SportSession? { controller.sessions.first { $0.id == sessionID } }

  var body: some View {
    ScrollView {
      if let s = session {
        VStack(alignment: .leading, spacing: 16) {
          header(s)
          if !s.hrSeries.isEmpty { hrChart(s) }
          if SportKind.of(s.type) == .musculation { setsSection(s) }
          if SportKind.of(s.type) == .tennis { tennisSection(s) }
          if SportKind.of(s.type) == .course, let c = s.cadence, c > 0 {
            HStack { metric("Cadence", "\(Int(c))", "pas/min"); metric("Durée", "\(Int(s.durationMin ?? 0))", "min") }
          }
          if !s.probs.isEmpty { probsSection(s) }
        }
        .padding(16)
        .frame(maxWidth: 640)
        .frame(maxWidth: .infinity)
      }
    }
    .gooseScreenBackground()
    .navigationTitle(SportKind.of(session?.type).title)
    .navigationBarTitleDisplayMode(.inline)
    .confirmationDialog("Supprimer cette séance ?", isPresented: $confirmDelete, titleVisibility: .visible) {
      Button("Supprimer", role: .destructive) { controller.delete(sessionID); dismiss() }
      Button("Annuler", role: .cancel) {}
    } message: {
      Text("Elle disparaît de l'historique et du jeu de données. Cette action est définitive.")
    }
    .alert("Nouvel exercice", isPresented: Binding(get: { newExerciseFor != nil || newExerciseGroup != nil }, set: { if !$0 { newExerciseFor = nil; newExerciseGroup = nil } })) {
      TextField("Nom", text: $newExercise)
      Button("Enregistrer") {
        let name = newExercise.trimmingCharacters(in: .whitespaces)
        if !name.isEmpty {
          for seg in newExerciseGroup ?? (newExerciseFor.map { [$0] } ?? []) { controller.annotate("segment", seg.id, "exercise", name) }
        }
        newExercise = ""
        newExerciseFor = nil
        newExerciseGroup = nil
      }
      Button("Annuler", role: .cancel) { newExerciseFor = nil; newExerciseGroup = nil }
    }
  }

  private func header(_ s: SportSession) -> some View {
    VStack(alignment: .leading, spacing: 12) {
      HStack {
        Menu {
          ForEach(SportKind.allCases) { k in Button(k.title) { controller.annotate("session", s.id, "type", k.rawValue) } }
          Button("Pas une séance") { controller.annotate("session", s.id, "reject") }
          Divider()
          Button("Supprimer la séance", systemImage: "trash", role: .destructive) { confirmDelete = true }
        } label: {
          Label(SportKind.of(s.type).title, systemImage: SportKind.of(s.type).icon).font(.title3.weight(.semibold))
        }
        Spacer()
        if !s.isValidated {
          Button("Valider") { controller.annotate("session", s.id, "confirm") }.buttonStyle(.borderedProminent)
        } else {
          Label("Validée", systemImage: "checkmark.seal.fill").foregroundStyle(.green).font(.subheadline)
        }
      }
      Text(timeRange(s)).font(.subheadline).foregroundStyle(.secondary)
      HStack(spacing: 10) {
        metric("Durée", "\(Int(s.durationMin ?? 0))", "min")
        metric("FC moy.", s.hrMean.map { "\(Int($0))" } ?? "–", "bpm")
        metric("FC max", s.hrMax.map { "\(Int($0))" } ?? "–", "bpm")
      }
    }
    .padding(16)
    .background(.background.secondary, in: RoundedRectangle(cornerRadius: 20, style: .continuous))
  }

  private func hrChart(_ s: SportSession) -> some View {
    Chart {
      ForEach(Array(s.hrSeries.enumerated()), id: \.offset) { _, p in
        LineMark(x: .value("Heure", p.0), y: .value("FC", p.1)).interpolationMethod(.monotone).foregroundStyle(.red)
      }
      ForEach(s.segments.filter { $0.kind == "set" || $0.kind == "exchange" }) { seg in
        RectangleMark(xStart: .value("Début", seg.start), xEnd: .value("Fin", seg.end)).foregroundStyle(SportKind.of(s.type).tint.opacity(0.18))
      }
    }
    .chartYScale(domain: .automatic(includesZero: false))
    .frame(height: 170)
    .padding(16)
    .background(.background.secondary, in: RoundedRectangle(cornerRadius: 20, style: .continuous))
  }

  @ViewBuilder private func setsSection(_ s: SportSession) -> some View {
    let groups = s.exerciseGroups
    VStack(alignment: .leading, spacing: 12) {
      HStack {
        Text("Exercices").font(.headline)
        Spacer()
        if let line = s.strengthLine { Text(line).font(.subheadline).foregroundStyle(.secondary) }
      }
      if groups.isEmpty {
        Text(s.imuMinutes ?? 0 > 0 ? "Aucune série répétitive trouvée" : "Pas de capteurs 100 Hz sur cette séance").font(.subheadline).foregroundStyle(.secondary)
      }
      ForEach(Array(groups.enumerated()), id: \.element.key) { gi, g in
        VStack(alignment: .leading, spacing: 8) {
          HStack(alignment: .firstTextBaseline) {
            let guessed = g.sets.allSatisfy { $0.exerciseLabel == nil }
            Menu {
              if let name = g.name, guessed {
                Button("Oui, \(name)", systemImage: "checkmark") { for set in g.sets { controller.annotate("segment", set.id, "exercise", name) } }
                Divider()
              }
              ForEach(controller.exercises.filter { $0 != g.name }, id: \.self) { e in
                Button(e) { for set in g.sets { controller.annotate("segment", set.id, "exercise", e) } }
              }
              Button("Nouvel exercice…") { newExerciseGroup = g.sets }
            } label: {
              HStack(spacing: 4) {
                Text(g.name.map { guessed ? "\($0) ?" : $0 } ?? "Exercice \(gi + 1) ?").font(.title3.weight(.semibold))
                Image(systemName: "chevron.up.chevron.down").font(.caption)
              }
              .foregroundStyle(guessed ? .orange : .primary)
            }
            Spacer()
            Text("\(g.sets.count) × " + g.sets.map { "\($0.reps ?? 0)" }.joined(separator: " / ")).font(.subheadline.monospacedDigit()).foregroundStyle(.secondary)
          }
          ForEach(Array(g.sets.enumerated()), id: \.element.id) { i, seg in
            SetRow(index: i + 1, seg: seg, exercises: controller.exercises, controller: controller) { newExerciseFor = seg }
          }
        }
        .padding(.bottom, 4)
      }
      let all = groups.flatMap(\.sets).sorted { $0.start < $1.start }
      let rests = zip(all, all.dropFirst()).map { $1.start.timeIntervalSince($0.end) }
      if !rests.isEmpty {
        Text("Repos médian entre séries \(Int(rests.sorted()[rests.count / 2])) s").font(.caption).foregroundStyle(.secondary)
      }
    }
  }

  @ViewBuilder private func tennisSection(_ s: SportSession) -> some View {
    let ex = s.segments.filter { $0.kind == "exchange" }
    VStack(alignment: .leading, spacing: 10) {
      Text("Frappes").font(.headline)
      HStack(spacing: 10) {
        metric("Frappes", "\(s.strokes ?? 0)", "détectées")
        metric("Échanges", "\(ex.count)", "blocs")
        metric("Par échange", ex.isEmpty ? "–" : String(format: "%.1f", Double(ex.compactMap(\.repsPred).reduce(0, +)) / Double(ex.count)), "frappes")
      }
    }
  }

  private func probsSection(_ s: SportSession) -> some View {
    VStack(alignment: .leading, spacing: 6) {
      Text("Probabilités").font(.headline)
      ForEach(SportKind.allCases) { k in
        HStack {
          Text(k.title).font(.subheadline).frame(width: 110, alignment: .leading)
          GeometryReader { g in
            Capsule().fill(k.tint.opacity(0.25)).overlay(alignment: .leading) {
              Capsule().fill(k.tint).frame(width: g.size.width * (s.probs[k.rawValue] ?? 0))
            }
          }.frame(height: 8)
          Text("\(Int(((s.probs[k.rawValue] ?? 0) * 100).rounded())) %").font(.caption.monospacedDigit()).frame(width: 44, alignment: .trailing)
        }
      }
      if let m = s.modelVersion { Text("Modèle \(m)").font(.caption2).foregroundStyle(.secondary) }
    }
    .padding(16)
    .background(.background.secondary, in: RoundedRectangle(cornerRadius: 20, style: .continuous))
  }
}

struct SetRow: View {
  let index: Int
  let seg: SportSegment
  let exercises: [String]
  @ObservedObject var controller: SportController
  let newExercise: () -> Void

  var body: some View {
    VStack(alignment: .leading, spacing: 10) {
      HStack(alignment: .firstTextBaseline) {
        Text("Série \(index)").font(.subheadline.weight(.semibold))
        Menu {
          ForEach(exercises, id: \.self) { e in Button(e) { controller.annotate("segment", seg.id, "exercise", e) } }
          Button("Nouvel exercice…", action: newExercise)
        } label: {
          HStack(spacing: 4) {
            Text(seg.exercise ?? "Exercice ?")
            Image(systemName: "chevron.up.chevron.down").font(.caption2)
          }
          .font(.subheadline)
          .foregroundStyle(seg.exercise == nil ? .orange : .primary)
        }
        Spacer()
        Text("\(Int(seg.end.timeIntervalSince(seg.start))) s").font(.caption).foregroundStyle(.secondary)
      }
      HStack(spacing: 12) {
        Button { controller.annotate("segment", seg.id, "reps", "\(max(0, (seg.reps ?? 0) - 1))") } label: { Image(systemName: "minus.circle.fill").font(.title2) }.buttonStyle(.plain)
        Text("\(seg.reps ?? 0)").font(.system(.title, design: .rounded).weight(.bold)).monospacedDigit().frame(minWidth: 36)
        Button { controller.annotate("segment", seg.id, "reps", "\((seg.reps ?? 0) + 1)") } label: { Image(systemName: "plus.circle.fill").font(.title2) }.buttonStyle(.plain)
        Text("rép.").font(.subheadline).foregroundStyle(.secondary)
        Spacer()
        if seg.isValidated {
          Image(systemName: "checkmark.circle.fill").foregroundStyle(.green).font(.title2)
        } else {
          Button { controller.annotate("segment", seg.id, "confirm") } label: { Image(systemName: "checkmark.circle").font(.title2) }.buttonStyle(.plain).foregroundStyle(.green)
        }
        Button { controller.annotate("segment", seg.id, "reject") } label: { Image(systemName: "xmark.circle").font(.title2) }.buttonStyle(.plain).foregroundStyle(.secondary)
      }
      if let up = seg.upS, let down = seg.downS {
        let concUp = seg.concentric == "up"
        HStack(spacing: 6) {
          Text("Concentrique \(String(format: "%.1f", concUp ? up : down)) s · excentrique \(String(format: "%.1f", concUp ? down : up)) s · pause \(String(format: "%.1f", seg.holdS ?? 0)) s")
            .font(.caption).foregroundStyle(.secondary)
          Spacer()
          Menu {
            Button("Concentrique en montée") { controller.annotate("segment", seg.id, "concentric", "up") }
            Button("Concentrique en descente") { controller.annotate("segment", seg.id, "concentric", "down") }
          } label: { Image(systemName: "arrow.up.arrow.down.circle").font(.subheadline) }
        }
      }
    }
    .padding(14)
    .background(.background.secondary, in: RoundedRectangle(cornerRadius: 16, style: .continuous))
  }
}

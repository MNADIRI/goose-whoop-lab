import Charts
import Foundation
import SwiftUI
import UserNotifications

// MARK: - Command gate

extension GooseBLEClient {
  /// Alarm / sensor writes are refused while a history sync runs or another alarm command is in
  /// flight; smart wake must never lose a command, so it waits for the strap to be free.
  var isFreeForCommands: Bool { canSendHello && !isHistoricalSyncing && pendingAlarmCommand == nil }

  func whenFreeForCommands(timeout: TimeInterval = 40, _ action: @escaping (GooseBLEClient) -> Void) {
    let deadline = Date().addingTimeInterval(timeout)
    func attempt() {
      if isFreeForCommands {
        action(self)
      } else if Date() < deadline {
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.5) { attempt() }
      } else {
        record(level: .warn, source: "smart_wake", title: "command.gave_up", body: "strap busy for \(Int(timeout)) s")
      }
    }
    attempt()
  }
}

// MARK: - Haptic ramp

/// Continuous, linearly increasing vibration built from the strap's haptic sequencer.
///
/// The alarm pattern (8 effect slots, 16-bit per-slot loop word, main loop, duration) matches a TI
/// DRV2625-style sequencer with the standard ROM library; WHOOP's default [47, 152] reads as
/// "buzz 100 % + 240 ms wait", i.e. pulses. Continuous levels come from the library's smooth / transition
/// hums: 123 = 10 %, 122 = 20 %, 121 = 30 %, 120 = 40 %, 119 = 50 %, 66 = 60 %, 65 = 80 %, 64 = 100 %.
/// A single pattern cannot hold a ramp longer than one sequence, so the app plays one constant-level,
/// gap-free pattern per step, each scheduled on the strap clock (SET_ALARM_TIME, alternating alarm
/// ids 2/3 so a step never overwrites the one playing). Step timing makes amplitude grow linearly
/// with time: 0 -> 100 % over 40 s, then a 100 % hold in 5 s chunks (so a stop takes effect quickly).
/// The library/encoding is an inference: the strap answers VALID_PATTERN / INVALID_EFFECT /
/// INVALID_LOOPS to every pattern, which the Alarm tab shows after a test.
@MainActor
final class HapticRampPlayer: ObservableObject {
  struct Step {
    let effect: UInt8
    let percent: Int
    let seconds: Int
  }

  /// Calibrated on this strap (10 Nov): every effect and loop word is accepted; with the main loop
  /// set to 7 (continuous) the duration must be >= 30 s, with a finite main loop (0-3) 8 s is valid.
  /// Steps therefore use main loop 3 and 5 s levels: 10 -> 100 % over 40 s, then a 100 % hold.
  static func steps(level: Int) -> [Step] {
    let ladder: [(UInt8, Int)] = [(123, 10), (122, 20), (121, 30), (120, 40), (119, 50), (66, 60), (65, 80)]
    let hold = Array(repeating: Step(effect: 64, percent: 100, seconds: 5), count: level == 3 ? 8 : 4)
    let from = level == 1 ? 0 : level == 2 ? 3 : 5
    return ladder[from...].map { Step(effect: $0.0, percent: $0.1, seconds: 5) } + [Step(effect: 64, percent: 100, seconds: 5)] + hold
  }

  /// gap-free constant level: 8 identical slots, each looped 4x (0xFFFF), main loop 3 (finite, so a
  /// short duration is accepted), duration = step + 1 s of overlap with the next step.
  static func pattern(_ s: Step) -> GooseBLEClient.AlarmHapticsPattern {
    GooseBLEClient.AlarmHapticsPattern(waveformEffects: Array(repeating: s.effect, count: 8), loopControl: 0xFFFF, overallLoop: 3, durationSeconds: UInt8(s.seconds + 1))
  }

  /// Single-pattern ascending sequence (validated: main loop 7 with 30-60 s). Used by the strap-side
  /// deadline alarm, and as the fallback if the strap ever rejects a ramp step.
  static let deadlinePattern = GooseBLEClient.AlarmHapticsPattern(waveformEffects: [123, 122, 121, 120, 119, 66, 65, 64], loopControl: 0xFFFF, overallLoop: 7, durationSeconds: 60)

  @Published private(set) var isPlaying = false
  @Published private(set) var percent = 0
  private var work: [DispatchWorkItem] = []
  private weak var ble: GooseBLEClient?

  func attach(ble: GooseBLEClient) { self.ble = ble }

  func play(level: Int) {
    stop(disableStrap: false)
    guard let ble else { return }
    isPlaying = true
    guard ble.isFreeForCommands else {
      // a history sync is running: wait for it, then start the ramp
      ble.whenFreeForCommands { [weak self] _ in
        guard let self, self.isPlaying else { return }
        self.isPlaying = false
        self.play(level: level)
      }
      return
    }
    let start = Date().addingTimeInterval(3)
    var offset = 0
    for (i, step) in Self.steps(level: level).enumerated() {
      let at = start.addingTimeInterval(TimeInterval(offset))
      // send ~2 s before the step starts; the strap starts it on its own clock
      let send = DispatchWorkItem { [weak self, weak ble] in
        ble?.setWhoopAlarm(at: at, alarmID: 2 + i % 2, pattern: Self.pattern(step))
        let show = DispatchWorkItem { self?.percent = step.percent }
        DispatchQueue.main.asyncAfter(deadline: .now() + max(0, at.timeIntervalSinceNow), execute: show)
      }
      work.append(send)
      DispatchQueue.main.asyncAfter(deadline: .now() + max(0, at.timeIntervalSinceNow - 2), execute: send)
      offset += step.seconds
    }
    // fallback: if the first step is rejected, play the validated single-pattern sequence instead
    let check = DispatchWorkItem { [weak self, weak ble] in
      guard let self, let ble, ble.lastAlarmResponseSummary.contains("FAILURE") else { return }
      self.work.forEach { $0.cancel() }
      self.work.removeAll()
      ble.setWhoopAlarm(at: Date().addingTimeInterval(2), alarmID: 2, pattern: Self.deadlinePattern)
      ble.record(source: "smart_wake", title: "ramp.fallback", body: ble.lastAlarmResponseSummary)
      DispatchQueue.main.asyncAfter(deadline: .now() + 62) { [weak self] in self?.isPlaying = false; self?.percent = 0 }
    }
    work.append(check)
    DispatchQueue.main.asyncAfter(deadline: .now() + 2.8, execute: check)
    let end = DispatchWorkItem { [weak self] in self?.isPlaying = false; self?.percent = 0 }
    work.append(end)
    DispatchQueue.main.asyncAfter(deadline: .now() + 3 + TimeInterval(offset), execute: end)
    ble.record(source: "smart_wake", title: "ramp.start", body: "level=\(level) total=\(offset)s")
  }

  /// Silent validation: schedules each candidate pattern 6 h ahead (alarm id 4) so the strap only
  /// answers VALID / INVALID_*; responses land in decoded_frames in this order. Then disables all.
  static let probes: [(String, GooseBLEClient.AlarmHapticsPattern)] = {
    func p(_ e: [UInt8], _ loop: UInt16, _ main: UInt8, _ d: UInt8) -> GooseBLEClient.AlarmHapticsPattern {
      GooseBLEClient.AlarmHapticsPattern(waveformEffects: e + Array(repeating: 0, count: 8 - e.count), loopControl: loop, overallLoop: main, durationSeconds: d)
    }
    let hum100: [UInt8] = Array(repeating: 64, count: 8)
    return [
      ("default 47/152 d30", p([47, 152], 0, 7, 30)),
      ("hum64x8 loop0 main7 d30", p(hum100, 0, 7, 30)),
      ("hum64x8 loopFFFF main7 d30", p(hum100, 0xFFFF, 7, 30)),
      ("hum64x8 loop0 main7 d20", p(hum100, 0, 7, 20)),
      ("hum64x8 loop0 main7 d15", p(hum100, 0, 7, 15)),
      ("hum64x8 loop0 main7 d10", p(hum100, 0, 7, 10)),
      ("hum64x8 loop0 main7 d5", p(hum100, 0, 7, 5)),
      ("hum64x8 loop0 main3 d30", p(hum100, 0, 3, 30)),
      ("smooth123x8 loop0 main7 d30", p(Array(repeating: 123, count: 8), 0, 7, 30)),
      ("ramp123..64 loop0 main7 d30", p([123, 122, 121, 120, 119, 66, 65, 64], 0, 7, 30)),
      ("ramp123..64 loop5555 main7 d30", p([123, 122, 121, 120, 119, 66, 65, 64], 0x5555, 7, 30)),
      ("long118 single d30", p([118], 0, 7, 30)),
      ("default 47/152 main2 d8", p([47, 152], 0, 2, 8)),
      ("hum64x8 loop0 main2 d8", p(hum100, 0, 2, 8)),
      ("hum64x8 loopFFFF main2 d8", p(hum100, 0xFFFF, 2, 8)),
      ("hum64x8 loop5555 main2 d8", p(hum100, 0x5555, 2, 8)),
      ("hum64x8 loop0 main0 d8", p(hum100, 0, 0, 8)),
      ("ramp123..64 loop0 main2 d8", p([123, 122, 121, 120, 119, 66, 65, 64], 0, 2, 8)),
      ("default 47/152 d60", p([47, 152], 0, 7, 60)),
      ("default 47/152 d120", p([47, 152], 0, 7, 120)),
    ]
  }()

  @Published private(set) var probeStatus = ""

  @Published private(set) var probeResults: [(name: String, response: String)] = []

  func probe() {
    guard let ble else { return }
    stop(disableStrap: false)
    probeResults = []
    let at = Date().addingTimeInterval(6 * 3600)
    for (i, item) in Self.probes.enumerated() {
      DispatchQueue.main.asyncAfter(deadline: .now() + Double(i) * 2.5) { [weak self, weak ble] in
        ble?.setWhoopAlarm(at: at, alarmID: 4, pattern: item.1)
        self?.probeStatus = "Calibration \(i + 1)/\(Self.probes.count) : \(item.0)"
      }
      // read the strap's answer just before the next probe
      DispatchQueue.main.asyncAfter(deadline: .now() + Double(i) * 2.5 + 2.2) { [weak self, weak ble] in
        guard let ble else { return }
        self?.probeResults.append((item.0, "\(ble.lastAlarmResponseSummary) | \(ble.lastAlarmResponsePayloadHex) | \(ble.alarmCommandStatus)"))
      }
    }
    DispatchQueue.main.asyncAfter(deadline: .now() + Double(Self.probes.count) * 2.5 + 1) { [weak self, weak ble] in
      ble?.disableWhoopAlarms()
      guard let self else { return }
      self.probeStatus = "Calibration terminée"
      let rows = self.probeResults.map { ["probe": $0.name, "response": $0.response] }
      if let dir = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask).first,
         let data = try? JSONSerialization.data(withJSONObject: rows, options: .prettyPrinted) {
        try? data.write(to: dir.appendingPathComponent("haptic_probe.json"))
      }
    }
  }

  func stop(disableStrap: Bool = true) {
    work.forEach { $0.cancel() }
    work.removeAll()
    if isPlaying && disableStrap { ble?.whenFreeForCommands { $0.disableWhoopAlarms() } }
    isPlaying = false
    percent = 0
  }
}

// MARK: - Motion latency probe

/// Measures how fast movement data can reach the phone: phase A = strap-pushed history
/// (ENTER_HIGH_FREQ_SYNC, 15 s interval), latency = arrival - R18 record timestamp; phase B =
/// realtime streams (TOGGLE_REALTIME_HR + SEND_R10_R11_REALTIME): which frames arrive, how often.
/// No vibration. Results: on screen and Documents/motion_probe.json.
@MainActor
final class MotionProbe: ObservableObject {
  @Published private(set) var status = ""
  @Published private(set) var lines: [String] = []
  private weak var ble: GooseBLEClient?
  private var phase = ""
  private var phaseStart = Date()
  private var arrivals: [String: [Double]] = [:]   // key = phase|type|layout|len -> arrival offsets
  private var samples: [String: [String]] = [:]
  private var r18Latency: [String: [Double]] = [:]

  func attach(ble: GooseBLEClient) { self.ble = ble }

  func run(keepLiveHR: Bool) {
    guard let ble else { return }
    lines = []; arrivals = [:]; samples = [:]; r18Latency = [:]
    ble.rawFrameSink = { [weak self] hex, date in
      Task { @MainActor in self?.observe(hex: hex, at: date) }
    }
    status = "Attente d'un bracelet libre…"
    // round 2: which high-frequency intervals the strap accepts, then realtime IMU mode
    let intervals = [30, 60, 120, 180]
    ble.whenFreeForCommands(timeout: 60) { [weak self] ble in
      guard let self else { return }
      var t = 0.0
      for iv in intervals {
        DispatchQueue.main.asyncAfter(deadline: .now() + t) {
          self.begin("A_hfs_\(iv)s")
          ble.whenFreeForCommands { $0.enterHighFrequencyHistorySync(intervalSeconds: iv, durationSeconds: 600) }
        }
        // stay long enough to see one push at this interval (max 70 s per try)
        t += Double(min(iv, 60) + 10)
        DispatchQueue.main.asyncAfter(deadline: .now() + t - 4) {
          ble.whenFreeForCommands { $0.exitHighFrequencyHistorySync() }
        }
      }
      DispatchQueue.main.asyncAfter(deadline: .now() + t + 2) {
        self.begin("B_imu_realtime")
        ble.whenFreeForCommands { $0.writeSensorStreamCommands([GooseBLEClient.SensorStreamCommandKind(commandNumber: 106, payload: GooseBLEClient.SensorStreamCommandKind.revisionBoolean(true), name: "TOGGLE_IMU_MODE_ON")], requestedStatus: "Motion probe IMU", updatePhysiologyStatus: false) }
        DispatchQueue.main.asyncAfter(deadline: .now() + 30) {
          ble.whenFreeForCommands { $0.writeSensorStreamCommands([GooseBLEClient.SensorStreamCommandKind(commandNumber: 106, payload: GooseBLEClient.SensorStreamCommandKind.revisionBoolean(false), name: "TOGGLE_IMU_MODE_OFF")], requestedStatus: "Motion probe IMU off", updatePhysiologyStatus: false) }
          DispatchQueue.main.asyncAfter(deadline: .now() + 3) { self.finish() }
        }
      }
    }
    _ = keepLiveHR
  }

  private func begin(_ p: String) {
    phase = p
    phaseStart = Date()
    status = "Phase \(p)…"
  }

  private func observe(hex: String, at date: Date) {
    guard !phase.isEmpty, hex.count >= 20 else { return }
    func byte(_ i: Int) -> Int { Int(hex.dropFirst(2 * i).prefix(2), radix: 16) ?? -1 }
    let type = byte(8), layout = byte(9), len = hex.count / 2
    let key = "\(phase)|type\(type)|layout\(layout)|len\(len)"
    arrivals[key, default: []].append(date.timeIntervalSince(phaseStart))
    if samples[key, default: []].count < 3 { samples[key, default: []].append(String(hex.prefix(160))) }
    if type == 47, layout == 18, len == 124 {
      // R18 timestamp: u32 at frame byte 15
      let ts = (15..<19).reversed().reduce(0) { $0 << 8 | byte($1) }
      r18Latency[phase, default: []].append(date.timeIntervalSince1970 - Double(ts))
    }
  }

  private func finish() {
    ble?.rawFrameSink = nil
    phase = ""
    var out: [[String: Any]] = []
    for (key, t) in arrivals.sorted(by: { $0.key < $1.key }) {
      let gaps = zip(t.dropFirst(), t).map { $0 - $1 }.sorted()
      let medianGap = gaps.isEmpty ? 0 : gaps[gaps.count / 2]
      out.append(["key": key, "frames": t.count, "median_gap_s": medianGap, "first_s": t.first ?? 0, "samples": samples[key] ?? []])
      lines.append("\(key): \(t.count) trames, écart médian \(String(format: "%.2f", medianGap)) s")
    }
    for (p, lat) in r18Latency.sorted(by: { $0.key < $1.key }) {
      let s = lat.sorted()
      let med = s[s.count / 2], minL = s.first ?? 0
      out.append(["key": "\(p)|r18_latency", "frames": s.count, "median_s": med, "min_s": minL, "max_s": s.last ?? 0])
      lines.append("\(p) retard R18 : médian \(Int(med)) s, min \(Int(minL)) s (\(s.count) enreg.)")
    }
    if let dir = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask).first,
       let data = try? JSONSerialization.data(withJSONObject: out, options: .prettyPrinted) {
      try? data.write(to: dir.appendingPathComponent("motion_probe.json"))
    }
    status = "Test terminé"
  }
}

// MARK: - Controller

/// Smart wake-up: runs the Rust engine (`smart_wake.update`, Rust/core/src/smart_wake.rs) every 30 s
/// on the live packet-40 HR / R-R stream, which also keeps the app running in the background
/// (bluetooth-central). The user only sets the latest wake time; the engine opens the window when
/// tonight's recommended sleep is covered (10-45 min before the deadline). Safety nets: strap-side
/// alarm at the deadline (works even if the phone app dies) and an iOS notification one minute later.
@MainActor
final class SmartWakeController: ObservableObject {
  struct Session: Codable {
    var id: String
    var startTs: Int
    var windowEnd: Int
  }

  @Published var latest: Date
  @Published private(set) var session: Session?
  @Published private(set) var status = "Inactif"
  @Published private(set) var posterior: [Double] = []
  @Published private(set) var favourableNext10: Double?
  @Published private(set) var engineState = "idle"
  @Published private(set) var windowStart: Date?
  @Published private(set) var feedbackPendingSessionID: String?
  @Published private(set) var plan: [String: Any] = [:]
  let ramp = HapticRampPlayer()
  let motionProbe = MotionProbe()

  private weak var ble: GooseBLEClient?
  private let queue = DispatchQueue(label: "smart-wake.engine", qos: .userInitiated)
  nonisolated(unsafe) private var pending: [Int: (hr: Double?, rr: [Double])] = [:]
  nonisolated private let lock = NSLock()
  private var lastUpdate = Date.distantPast
  private var lastSyncRequest = Date.distantPast
  private var running = false
  private var acknowledged = false
  private let defaults = UserDefaults.standard
  private static let latestKey = "smartWake.latest"
  private static let sessionKey = "smartWake.session.v2"
  private static let feedbackKey = "smartWake.feedbackPending"
  private static let notificationID = "smart-wake-deadline"

  init() {
    let base = Calendar.current.startOfDay(for: Date())
    latest = (defaults.object(forKey: Self.latestKey) as? Date) ?? Calendar.current.date(byAdding: .minute, value: 7 * 60, to: base)!
    session = (defaults.data(forKey: Self.sessionKey)).flatMap { try? JSONDecoder().decode(Session.self, from: $0) }
    feedbackPendingSessionID = defaults.string(forKey: Self.feedbackKey)
    if session != nil { status = "Armé (repris)" }
  }

  var deadline: Date { GooseBLEClient.nextFutureAlarmDate(from: latest) }

  private var heartbeat: Timer?

  /// Fallback clock (the live stream normally drives the loop); also restarts setup after a relaunch.
  private func startHeartbeat() {
    heartbeat?.invalidate()
    let t = Timer(timeInterval: 15, repeats: true) { [weak self] _ in Task { @MainActor in self?.tick() } }
    t.tolerance = 5
    RunLoop.main.add(t, forMode: .common)
    heartbeat = t
  }

  func attach(ble: GooseBLEClient) {
    self.ble = ble
    ramp.attach(ble: ble)
    motionProbe.attach(ble: ble)
    if session != nil { startHeartbeat() }
    ble.liveSampleSink = { [weak self] date, hr, rr, source in
      self?.ingest(date: date, hr: hr, rr: rr, source: source)
    }
    ble.liveIMUSink = { [weak self] hex, date in
      self?.ingestIMU(hex: hex, date: date)
    }
  }

  // MARK: plan

  func refreshPlan() {
    defaults.set(latest, forKey: Self.latestKey)
    let args: [String: Any] = ["database_path": HealthDataStore.defaultDatabasePath(), "latest_wake": Int(deadline.timeIntervalSince1970), "utc_offset_seconds": TimeZone.current.secondsFromGMT()]
    queue.async { [weak self] in
      let r = (try? GooseRustBridge().request(method: "smart_wake.plan", args: args)) ?? [:]
      Task { @MainActor in self?.plan = r }
    }
  }

  // MARK: arming

  func arm() {
    guard let ble else { return }
    defaults.set(latest, forKey: Self.latestKey)
    let end = deadline
    let now = Date()
    let s = Session(id: "sw-\(Int(now.timeIntervalSince1970))", startTs: Int(now.timeIntervalSince1970), windowEnd: Int(end.timeIntervalSince1970))
    session = s
    acknowledged = false
    windowStart = nil
    defaults.set(try? JSONEncoder().encode(s), forKey: Self.sessionKey)
    deadlineConfirmed = false
    lastDeadlineAttempt = .distantPast
    lastLiveRequest = .distantPast
    ensureStrapSetup(now: now)
    startHeartbeat()
    scheduleBackupNotification(at: end.addingTimeInterval(60))
    status = "Armé : réveil au plus tard à \(end.formatted(date: .omitted, time: .shortened))"
    ble.record(source: "smart_wake", title: "armed", body: "deadline=\(s.windowEnd)")
  }

  func disarm(reason: String = "désactivé") {
    ramp.stop(disableStrap: false)
    session = nil
    windowStart = nil
    heartbeat?.invalidate()
    heartbeat = nil
    defaults.removeObject(forKey: Self.sessionKey)
    UNUserNotificationCenter.current().removePendingNotificationRequests(withIdentifiers: [Self.notificationID])
    // must not be lost (otherwise the deadline alarm still rings): wait for the strap to be free
    ble?.whenFreeForCommands(timeout: 120) { ble in
      ble.disableWhoopAlarms()
      DispatchQueue.main.asyncAfter(deadline: .now() + 2) {
        ble.whenFreeForCommands { $0.writeSensorStreamCommands([
          GooseBLEClient.SensorStreamCommandKind(commandNumber: 106, payload: GooseBLEClient.SensorStreamCommandKind.revisionBoolean(false), name: "TOGGLE_IMU_MODE_OFF"),
          GooseBLEClient.SensorStreamCommandKind(commandNumber: 3, payload: [0], name: "TOGGLE_REALTIME_HR_OFF"),
        ], requestedStatus: "Smart wake: live streams off", updatePhysiologyStatus: false) }
      }
    }
    status = "Inactif (\(reason))"
    engineState = "idle"
  }

  func acknowledgeAwake() {
    acknowledged = true
    ramp.stop()
    forceUpdate()
  }

  func testVibration() {
    ramp.play(level: 1)
    status = "Test : rampe continue de 10 % à 100 % en 40 s, puis maintien 20 s"
  }

  func submitFeedback(rating: Int) {
    guard let id = feedbackPendingSessionID else { return }
    let args: [String: Any] = ["database_path": HealthDataStore.defaultDatabasePath(), "session_id": id, "rating": rating]
    queue.async { _ = try? GooseRustBridge().request(method: "smart_wake.feedback", args: args) }
    feedbackPendingSessionID = nil
    defaults.removeObject(forKey: Self.feedbackKey)
  }

  // MARK: live loop

  nonisolated(unsafe) private var pendingIMU: [(Int, String)] = []
  nonisolated(unsafe) private var lastIMU: TimeInterval = 0

  nonisolated private func ingestIMU(hex: String, date: Date) {
    lock.lock()
    lastIMU = date.timeIntervalSince1970
    if pendingIMU.count < 600 { pendingIMU.append((Int(date.timeIntervalSince1970), hex)) }
    lock.unlock()
  }

  nonisolated(unsafe) private var lastPacket40: TimeInterval = 0
  nonisolated(unsafe) private var lastLiveSample: TimeInterval = 0

  nonisolated private func ingest(date: Date, hr: Int?, rr: [Double], source: String) {
    let ts = Int(date.timeIntervalSince1970)
    lock.lock()
    lastLiveSample = date.timeIntervalSince1970
    if source == "whoop.packet40" { lastPacket40 = date.timeIntervalSince1970 }
    // one R-R source only: the standard HR profile duplicates packet 40 when both are on
    let useRR = source == "whoop.packet40" || date.timeIntervalSince1970 - lastPacket40 > 5
    var e = pending[ts] ?? (nil, [])
    if let hr { e.hr = Double(hr) }
    if useRR { e.rr.append(contentsOf: rr) }
    pending[ts] = e
    lock.unlock()
    Task { @MainActor in self.tick() }
  }

  @Published private(set) var deadlineConfirmed = false
  @Published private(set) var liveStreamOK = false
  @Published private(set) var imuStreamOK = false
  private var lastIMURequest = Date.distantPast
  private var imuRequested = false
  private var lastDeadlineAttempt = Date.distantPast
  private var lastLiveRequest = Date.distantPast

  /// Keeps the strap configured all night: deadline alarm confirmed by the strap (SUCCESS), and the
  /// live HR stream re-enabled whenever it stops (e.g. after a reconnection).
  private func ensureStrapSetup(now: Date = Date()) {
    guard let ble, let s = session else { return }
    lock.lock(); let liveAge = now.timeIntervalSince1970 - lastLiveSample; lock.unlock()
    liveStreamOK = liveAge < 15
    if !liveStreamOK, now.timeIntervalSince(lastLiveRequest) >= 30, ble.isFreeForCommands {
      lastLiveRequest = now
      ble.writeSensorStreamCommands([GooseBLEClient.SensorStreamCommandKind(commandNumber: 3, payload: [1], name: "TOGGLE_REALTIME_HR_ON")], requestedStatus: "Smart wake: live HR", updatePhysiologyStatus: false)
    }
    // live IMU (1 frame/s, ~1 s latency) only from 5 min before the window opens: battery
    lock.lock(); let imuAge = now.timeIntervalSince1970 - lastIMU; lock.unlock()
    imuStreamOK = imuAge < 10
    let opens = windowStart ?? Date(timeIntervalSince1970: TimeInterval(s.windowEnd - 2700))
    if now >= opens.addingTimeInterval(-300), !imuStreamOK, now.timeIntervalSince(lastIMURequest) >= 30, ble.isFreeForCommands, !ramp.isPlaying {
      lastIMURequest = now
      imuRequested = true
      ble.writeSensorStreamCommands([GooseBLEClient.SensorStreamCommandKind(commandNumber: 106, payload: GooseBLEClient.SensorStreamCommandKind.revisionBoolean(true), name: "TOGGLE_IMU_MODE_ON")], requestedStatus: "Smart wake: live IMU", updatePhysiologyStatus: false)
    }
    if !deadlineConfirmed, now.timeIntervalSince(lastDeadlineAttempt) >= 20, ble.isFreeForCommands, !ramp.isPlaying {
      lastDeadlineAttempt = now
      let end = Date(timeIntervalSince1970: TimeInterval(s.windowEnd))
      ble.setWhoopAlarm(at: end, alarmID: 1, pattern: HapticRampPlayer.deadlinePattern)
      DispatchQueue.main.asyncAfter(deadline: .now() + 3) { [weak self, weak ble] in
        guard let self, let ble else { return }
        if ble.lastAlarmResponseSummary.contains("SET_ALARM_TIME"), ble.lastAlarmResponseSummary.contains("SUCCESS") {
          self.deadlineConfirmed = true
          ble.record(source: "smart_wake", title: "deadline.confirmed", body: "\(s.windowEnd)")
        }
      }
    }
  }

  private func tick(now: Date = Date()) {
    guard let s = session else {
      lock.lock(); pending.removeAll(); lock.unlock()
      return
    }
    ensureStrapSetup(now: now)
    let nearWindow = Int(now.timeIntervalSince1970) >= (windowStart.map { Int($0.timeIntervalSince1970) } ?? s.windowEnd - 2700) - 300
    // R18 (movement, band state incl. "up") only arrives through a history sync; never while a
    // ramp is playing (alarm writes are blocked during a sync)
    let interval: TimeInterval = engineState == "stimulating" ? 45 : nearWindow ? 120 : 600
    if let ble, ble.canSyncHistorical, !ramp.isPlaying, now.timeIntervalSince(lastSyncRequest) >= interval {
      lastSyncRequest = now
      ble.syncHistoricalPackets()
    }
    guard now.timeIntervalSince(lastUpdate) >= 30 else { return }
    forceUpdate(now: now)
  }

  private func forceUpdate(now: Date = Date()) {
    guard let s = session, !running else { return }
    running = true
    lastUpdate = now
    lock.lock()
    let live = pending.sorted { $0.key < $1.key }.map { [$0.key, $0.value.hr ?? 0, $0.value.rr] as [Any] }
    pending.removeAll()
    let imu = pendingIMU.map { [$0.0, $0.1] as [Any] }
    pendingIMU.removeAll()
    lock.unlock()
    let args: [String: Any] = [
      "database_path": HealthDataStore.defaultDatabasePath(),
      "session": ["id": s.id, "start_ts": s.startTs, "window_start": 0, "window_end": s.windowEnd, "utc_offset_seconds": TimeZone.current.secondsFromGMT()],
      "live": live,
      "imu": imu,
      "acknowledged": acknowledged,
    ]
    queue.async { [weak self] in
      let result = try? GooseRustBridge().request(method: "smart_wake.update", args: args)
      Task { @MainActor in self?.apply(result) }
    }
  }

  private func apply(_ r: [String: Any]?) {
    running = false
    guard let r, let s = session else {
      if session != nil { status = "Moteur indisponible" }
      return
    }
    engineState = r["state"] as? String ?? engineState
    if let w = r["window"] as? [String: Any], let ws = w["start"] as? Int {
      windowStart = Date(timeIntervalSince1970: TimeInterval(ws))
    }
    if let epoch = r["epoch"] as? [String: Any] {
      posterior = epoch["posterior"] as? [Double] ?? posterior
      favourableNext10 = (epoch["decision"] as? [String: Any])?["p_light_or_wake_next_10min"] as? Double
    }
    switch r["action"] as? String {
    case "stimulate":
      let level = r["level"] as? Int ?? 1
      ramp.play(level: level)
      status = "Réveil en cours (niveau \(level)) : \(r["why"] as? String ?? "")"
      ble?.record(source: "smart_wake", title: "stimulate", body: "level=\(level) why=\(r["why"] ?? "")")
    case "awake":
      ramp.stop()
      ble?.record(source: "smart_wake", title: "awake", body: "\(s.id) \(r["why"] ?? "")")
      feedbackPendingSessionID = s.id
      defaults.set(s.id, forKey: Self.feedbackKey)
      disarm(reason: "réveillé")
      status = "Réveillé (\(r["why"] as? String ?? "confirmé")). Bonne journée !"
    case "deadline":
      status = "Heure limite : alarme du bracelet"
    default:
      if engineState == "armed" {
        if let ws = windowStart, Date() >= ws {
          status = "Fenêtre ouverte : recherche du meilleur moment"
        } else {
          status = "Surveillance du sommeil"
        }
      }
    }
  }

  private func scheduleBackupNotification(at date: Date) {
    let center = UNUserNotificationCenter.current()
    center.requestAuthorization(options: [.alert, .sound]) { granted, _ in
      guard granted else { return }
      let content = UNMutableNotificationContent()
      content.title = "Réveil"
      content.body = "Heure limite de ta fenêtre de réveil."
      content.sound = .default
      let comps = Calendar.current.dateComponents([.year, .month, .day, .hour, .minute], from: date)
      center.add(UNNotificationRequest(identifier: Self.notificationID, content: content, trigger: UNCalendarNotificationTrigger(dateMatching: comps, repeats: false)))
    }
  }
}

// MARK: - Alarm tab

struct AlarmView: View {
  @ObservedObject var controller: SmartWakeController
  @ObservedObject private var ble: GooseBLEClient
  @ObservedObject private var ramp: HapticRampPlayer
  @State private var showPicker = false

  init(controller: SmartWakeController, ble: GooseBLEClient) {
    self.controller = controller
    _ramp = ObservedObject(wrappedValue: controller.ramp)
    _ble = ObservedObject(wrappedValue: ble)
  }

  var body: some View {
    ScrollView {
      VStack(spacing: 18) {
        hero
        if controller.feedbackPendingSessionID != nil { feedbackCard }
        if controller.session != nil { liveCard }
        tonightCard
        nightsCard
        vibrationCard
        MotionProbeCard(probe: controller.motionProbe, keepLiveHR: controller.session != nil)
        Text("Estimations expérimentales : probabilités d'état estimées, moment favorable prédit. Méthode : docs/SMART_WAKE.md")
          .font(.caption2).foregroundStyle(.secondary).multilineTextAlignment(.center)
      }
      .padding(.horizontal, 16)
      .padding(.vertical, 12)
      .frame(maxWidth: 640)
      .frame(maxWidth: .infinity)
    }
    .gooseScreenBackground()
    .navigationTitle("Alarme")
    .navigationBarTitleDisplayMode(.inline)
    .onAppear { controller.refreshPlan() }
    .onChange(of: controller.latest) { _, _ in controller.refreshPlan() }
    .refreshable { controller.refreshPlan() }
  }

  // MARK: hero

  private var hero: some View {
    VStack(spacing: 14) {
      HStack {
        Label(controller.session == nil ? "Réveil intelligent" : "Armé", systemImage: controller.session == nil ? "moon.zzz" : "alarm.waves.left.and.right.fill")
          .font(.subheadline.weight(.semibold))
          .foregroundStyle(.white.opacity(0.85))
        Spacer()
        Toggle("", isOn: Binding(get: { controller.session != nil }, set: { $0 ? controller.arm() : controller.disarm() }))
          .labelsHidden().tint(.cyan)
      }
      Button { if controller.session == nil { showPicker.toggle() } } label: {
        VStack(spacing: 2) {
          Text("Au plus tard").font(.caption).foregroundStyle(.white.opacity(0.7))
          Text(controller.latest, format: .dateTime.hour().minute())
            .font(.system(size: 64, weight: .thin, design: .rounded))
            .foregroundStyle(.white)
            .contentTransition(.numericText())
        }
      }
      .buttonStyle(.plain)
      .disabled(controller.session != nil)
      if showPicker && controller.session == nil {
        DatePicker("", selection: $controller.latest, displayedComponents: .hourAndMinute)
          .datePickerStyle(.wheel).labelsHidden().colorScheme(.dark).frame(maxHeight: 140)
      }
      HStack(spacing: 8) {
        Image(systemName: "wand.and.stars")
        Text(windowText).font(.footnote)
      }
      .foregroundStyle(.white.opacity(0.85))
      .padding(.horizontal, 12).padding(.vertical, 8)
      .background(.white.opacity(0.12), in: Capsule())
      Text(controller.status).font(.caption).foregroundStyle(.white.opacity(0.7)).multilineTextAlignment(.center)
    }
    .padding(20)
    .frame(maxWidth: .infinity)
    .background(
      LinearGradient(colors: [Color(red: 0.10, green: 0.10, blue: 0.30), Color(red: 0.22, green: 0.12, blue: 0.42), Color(red: 0.05, green: 0.25, blue: 0.40)], startPoint: .topLeading, endPoint: .bottomTrailing),
      in: RoundedRectangle(cornerRadius: 28, style: .continuous)
    )
    .shadow(color: .black.opacity(0.25), radius: 16, y: 8)
    .animation(.snappy, value: showPicker)
  }

  private var windowText: String {
    if let ws = controller.windowStart {
      return "Fenêtre calculée : \(ws.formatted(date: .omitted, time: .shortened)) – \(controller.deadline.formatted(date: .omitted, time: .shortened))"
    }
    if let w = plan("expected_wake") as? [String: Any], let s = w["window_start"] as? Int {
      return "Fenêtre prévue : \(clock(s)) – \(controller.deadline.formatted(date: .omitted, time: .shortened)), ajustée pendant la nuit"
    }
    return "La fenêtre s'ouvre quand ton besoin de sommeil est couvert"
  }

  // MARK: live

  private var liveCard: some View {
    card(title: "Cette nuit", icon: "waveform.path.ecg") {
      HStack(spacing: 14) {
        Label(controller.liveStreamOK ? "FC en direct" : "FC en attente", systemImage: controller.liveStreamOK ? "dot.radiowaves.left.and.right" : "antenna.radiowaves.left.and.right.slash")
          .foregroundStyle(controller.liveStreamOK ? .green : .orange)
        Label(controller.deadlineConfirmed ? "Alarme limite sur le bracelet" : "Alarme limite en cours d'envoi", systemImage: controller.deadlineConfirmed ? "checkmark.seal.fill" : "hourglass")
          .foregroundStyle(controller.deadlineConfirmed ? .green : .orange)
      }
      .font(.caption)
      Label(controller.imuStreamOK ? "Mouvement en direct (IMU)" : "Mouvement en direct : activé 5 min avant la fenêtre", systemImage: controller.imuStreamOK ? "figure.walk.motion" : "moon.zzz")
        .font(.caption)
        .foregroundStyle(controller.imuStreamOK ? .green : .secondary)
      if controller.posterior.count == 4 {
        HStack(alignment: .bottom, spacing: 12) {
          stateBar("Éveil", controller.posterior[0], .orange)
          stateBar("Léger", controller.posterior[1], .cyan)
          stateBar("Profond", controller.posterior[2], .indigo)
          stateBar("Paradoxal", controller.posterior[3], .purple)
        }
        if let p = controller.favourableNext10 {
          ProgressView(value: p) { Text("Moment favorable dans les 10 min").font(.caption) } currentValueLabel: { Text("\(Int(p * 100)) %").font(.caption.monospacedDigit()) }
            .tint(.cyan)
        }
      } else {
        Text("En attente des premières données du bracelet…").font(.subheadline).foregroundStyle(.secondary)
      }
      if controller.engineState == "stimulating" || ramp.isPlaying {
        HStack {
          Label("Vibration \(ramp.percent) %", systemImage: "waveform").font(.subheadline.monospacedDigit())
          Spacer()
          Button("Je suis réveillé") { controller.acknowledgeAwake() }.buttonStyle(.borderedProminent).tint(.cyan)
        }
      }
    }
  }

  // MARK: tonight

  private var tonightCard: some View {
    card(title: "Ce soir", icon: "bed.double") {
      if let rec = plan("recommended_sleep_min") as? Double, let bed = plan("bedtime") as? Int {
        HStack(alignment: .top) {
          metric("Au lit à", clock(bed), "conseillé", .indigo)
          Spacer()
          metric("Sommeil", duration(rec), "recommandé", .cyan)
          Spacer()
          if let w = plan("expected_wake") as? [String: Any], let m = w["most_likely"] as? Int {
            metric("Réveil", clock(m), "prévu", .orange)
          }
        }
        if let lo = plan("lights_out_advice") as? Int, let lat = plan("latency_min") as? Double {
          Text("Commence à décrocher vers \(clock(lo)) : tu mets en général \(Int(lat)) min à t'endormir.")
            .font(.footnote).foregroundStyle(.secondary)
        }
        if let c = plan("components") as? [String: Any] {
          ViewThatFits(in: .horizontal) {
            HStack(spacing: 8) { chips(c) }
            VStack(alignment: .leading, spacing: 8) { chips(c) }
          }
        }
        if let note = plan("circadian_note") as? String {
          Label(note, systemImage: "exclamationmark.triangle").font(.footnote).foregroundStyle(.orange)
        }
        if let now = plan("if_bed_now") as? [String: Any], let possible = now["sleep_possible_min"] as? Double, let deficit = now["deficit_min"] as? Double {
          Divider()
          HStack {
            Image(systemName: "clock.arrow.circlepath").foregroundStyle(.secondary)
            Text(deficit > 0 ? "Couché maintenant : \(duration(possible)) possibles, \(duration(deficit)) de moins que conseillé." : "Couché maintenant : \(duration(possible)) possibles, ton besoin est couvert.")
              .font(.footnote)
          }
        }
      } else {
        Text("Calcul du plan…").foregroundStyle(.secondary)
      }
    }
  }

  @ViewBuilder
  private func chips(_ c: [String: Any]) -> some View {
    chip("Base", c["base_min"] as? Double, absolute: true, .gray)
    chip("Activité", c["activity_min"] as? Double, absolute: false, .green)
    chip("Dette", c["debt_min"] as? Double, absolute: false, .red)
    chip("Profond", c["deep_min"] as? Double, absolute: false, .indigo)
  }

  // MARK: nights

  private struct NightBar: Identifiable {
    let id: Int
    let label: String
    let kind: String
    let hours: Double
  }

  private var nightsCard: some View {
    card(title: "Dernières nuits", icon: "chart.bar") {
      let nights = (plan("recent_nights") as? [[String: Any]] ?? []).reversed()
      let bars: [NightBar] = nights.enumerated().flatMap { i, n -> [NightBar] in
        let label = (n["wake_time"] as? Int).map { Date(timeIntervalSince1970: TimeInterval($0)).formatted(.dateTime.weekday(.abbreviated)) } ?? "\(i)"
        let asleep = (n["asleep_min"] as? Double ?? 0) / 60
        let deep = (n["deep_min"] as? Double ?? 0) / 60
        return [NightBar(id: i * 2, label: label, kind: "Profond", hours: deep), NightBar(id: i * 2 + 1, label: label, kind: "Autre sommeil", hours: max(0, asleep - deep))]
      }
      if bars.isEmpty {
        Text("Pas encore de nuit analysée.").foregroundStyle(.secondary)
      } else {
        Chart(bars) { b in
          BarMark(x: .value("Nuit", b.label), y: .value("Heures", b.hours))
            .foregroundStyle(by: .value("Type", b.kind))
            .cornerRadius(4)
        }
        .chartForegroundStyleScale(["Profond": Color.indigo, "Autre sommeil": Color.cyan.opacity(0.5)])
        .chartYAxisLabel("h")
        .frame(height: 150)
        if let rec = plan("recommended_sleep_min") as? Double {
          Text("Objectif ce soir : \(duration(rec)).").font(.caption).foregroundStyle(.secondary)
        }
      }
    }
  }

  // MARK: vibration / feedback

  private var vibrationCard: some View {
    card(title: "Vibration", icon: "waveform") {
      Text("Modulation continue, démarrage à peine perceptible puis rampe linéaire jusqu'à 100 % en 40 s. Si aucun réveil n'est détecté, la rampe reprend plus haut, puis plus vite. Elle s'arrête dès que tu te lèves (état « levé » du bracelet), à un double tap, ou avec le bouton.")
        .font(.footnote).foregroundStyle(.secondary)
      HStack {
        Button(ramp.isPlaying ? "Arrêter" : "Tester la rampe") { ramp.isPlaying ? ramp.stop() : controller.testVibration() }
          .buttonStyle(.borderedProminent).tint(ramp.isPlaying ? .red : .indigo)
        if ramp.isPlaying {
          ProgressView(value: Double(ramp.percent), total: 100).tint(.indigo)
          Text("\(ramp.percent) %").font(.caption.monospacedDigit())
        }
      }
      Button("Calibrer (silencieux, ~30 s)") { ramp.probe() }
        .font(.footnote)
      if !ramp.probeStatus.isEmpty {
        Text(ramp.probeStatus).font(.caption2).foregroundStyle(.secondary)
        ForEach(Array(ramp.probeResults.enumerated()), id: \.offset) { _, r in
          Text("\(r.name) → \(r.response)").font(.system(size: 9, design: .monospaced)).foregroundStyle(.secondary)
        }
      }
      Text("Réponse du bracelet : \(ble.lastAlarmResponseSummary)").font(.caption2).foregroundStyle(.secondary)
    }
  }

  private var feedbackCard: some View {
    card(title: "Ton réveil", icon: "sun.horizon") {
      Text("Comment t'es-tu senti au réveil ?").font(.subheadline)
      HStack(spacing: 10) {
        ForEach(1...5, id: \.self) { r in
          Button { controller.submitFeedback(rating: r) } label: {
            Text(["😵", "😪", "😐", "🙂", "😃"][r - 1]).font(.title2).frame(maxWidth: .infinity, minHeight: 44)
          }
          .buttonStyle(.bordered)
        }
      }
      Text("Tes notes affinent le choix du moment après 8 matins.").font(.caption2).foregroundStyle(.secondary)
    }
  }

  // MARK: building blocks

  private func plan(_ key: String) -> Any? { controller.plan[key] }

  private func card<C: View>(title: String, icon: String, @ViewBuilder content: () -> C) -> some View {
    VStack(alignment: .leading, spacing: 12) {
      Label(title, systemImage: icon).font(.headline)
      content()
    }
    .padding(16)
    .frame(maxWidth: .infinity, alignment: .leading)
    .background(.background.secondary, in: RoundedRectangle(cornerRadius: 22, style: .continuous))
  }

  private func metric(_ title: String, _ value: String, _ caption: String, _ color: Color) -> some View {
    VStack(alignment: .leading, spacing: 2) {
      Text(title).font(.caption).foregroundStyle(.secondary)
      Text(value).font(.system(.title, design: .rounded).weight(.semibold)).foregroundStyle(color).minimumScaleFactor(0.6).lineLimit(1)
      Text(caption).font(.caption2).foregroundStyle(.secondary)
    }
  }

  private func chip(_ label: String, _ minutes: Double?, absolute: Bool, _ color: Color) -> some View {
    let m = Int(minutes ?? 0)
    return Text(absolute ? "\(label) \(duration(Double(m)))" : "\(label) +\(m) min")
      .font(.caption.weight(.medium))
      .padding(.horizontal, 10).padding(.vertical, 6)
      .background(color.opacity(0.15), in: Capsule())
      .foregroundStyle(color == .gray ? .primary : color)
  }

  private func stateBar(_ label: String, _ v: Double, _ color: Color) -> some View {
    VStack(spacing: 4) {
      GeometryReader { geo in
        ZStack(alignment: .bottom) {
          RoundedRectangle(cornerRadius: 6).fill(color.opacity(0.15))
          RoundedRectangle(cornerRadius: 6).fill(color.gradient).frame(height: max(4, geo.size.height * v))
        }
      }
      .frame(height: 64)
      .animation(.smooth, value: v)
      Text(label).font(.caption2)
      Text("\(Int(v * 100)) %").font(.caption2.monospacedDigit()).foregroundStyle(.secondary)
    }
    .frame(maxWidth: .infinity)
  }

  private func clock(_ ts: Int) -> String {
    Date(timeIntervalSince1970: TimeInterval(ts)).formatted(date: .omitted, time: .shortened)
  }

  private func duration(_ minutes: Double) -> String {
    let m = Int(minutes.rounded())
    return m >= 60 ? "\(m / 60) h \(String(format: "%02d", m % 60))" : "\(m) min"
  }
}

struct MotionProbeCard: View {
  @ObservedObject var probe: MotionProbe
  let keepLiveHR: Bool

  var body: some View {
    VStack(alignment: .leading, spacing: 8) {
      Label("Mouvement en direct (test)", systemImage: "figure.walk.motion").font(.headline)
      Text("Mesure le retard du mouvement : synchro haute fréquence à 30, 60, 120 et 180 s, puis mode IMU temps réel. Sans vibration, environ 4 min, bracelet porté.")
        .font(.footnote).foregroundStyle(.secondary)
      Button("Lancer le test") { probe.run(keepLiveHR: keepLiveHR) }.buttonStyle(.bordered)
      if !probe.status.isEmpty { Text(probe.status).font(.caption) }
      ForEach(probe.lines, id: \.self) { Text($0).font(.system(size: 10, design: .monospaced)).foregroundStyle(.secondary) }
    }
    .padding(16)
    .frame(maxWidth: .infinity, alignment: .leading)
    .background(.background.secondary, in: RoundedRectangle(cornerRadius: 22, style: .continuous))
  }
}

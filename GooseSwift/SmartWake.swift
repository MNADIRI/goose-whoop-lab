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
  /// Called whenever this player sends DISABLE_ALARM (it clears every strap alarm, the deadline
  /// alarm included), so the owner can re-send the deadline alarm.
  var onStrapAlarmsDisabled: (() -> Void)?

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
        // a late send must be dropped: SET_ALARM_TIME takes a clock time, and a time already
        // passed would be scheduled for tomorrow by nextFutureAlarmDate
        guard at.timeIntervalSinceNow > 0.5 else {
          ble?.record(level: .warn, source: "smart_wake", title: "ramp.step_skipped", body: "step \(i) late by \(Int(-at.timeIntervalSinceNow)) s")
          return
        }
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

  func stop(disableStrap: Bool = true) {
    work.forEach { $0.cancel() }
    work.removeAll()
    if isPlaying && disableStrap {
      ble?.whenFreeForCommands { [weak self] in
        $0.disableWhoopAlarms()
        self?.onStrapAlarmsDisabled?()
      }
    }
    isPlaying = false
    percent = 0
  }
}

// MARK: - Controller

/// Smart wake-up: runs the Rust engine (`smart_wake.update`, Rust/core/src/smart_wake.rs) every 30 s
/// on the live packet-40 HR / R-R stream, which also keeps the app running in the background
/// (bluetooth-central). The user sets the latest wake time and a mode: "Heure fixe" (no
/// anticipation) or "Fenêtre flexible" with a maximum anticipation. Sleep duration comes first: on
/// a short night, a high debt or poor data the engine keeps the deadline. The deadline never
/// depends on this app running: strap-side alarm at the deadline (sent at arming, re-sent until
/// the strap answers) and an iOS notification scheduled at arming for one minute later.
@MainActor
final class SmartWakeController: ObservableObject {
  struct Session: Codable {
    var id: String
    var startTs: Int
    var windowEnd: Int
    /// nil in sessions saved before the modes existed (engine default)
    var maxAdvanceMin: Int?
  }

  enum Mode: String, CaseIterable, Identifiable {
    case fixed, flexible
    var id: String { rawValue }
    var title: String { self == .fixed ? "Heure fixe" : "Fenêtre flexible" }
  }

  static let advanceChoices = [10, 20, 30, 45, 60]

  @Published var latest: Date
  @Published var mode: Mode = Mode(rawValue: UserDefaults.standard.string(forKey: "smartWake.mode") ?? "") ?? .flexible {
    didSet { UserDefaults.standard.set(mode.rawValue, forKey: "smartWake.mode") }
  }
  @Published var maxAdvanceMin: Int = (UserDefaults.standard.object(forKey: "smartWake.maxAdvance") as? Int) ?? 30 {
    didSet { UserDefaults.standard.set(maxAdvanceMin, forKey: "smartWake.maxAdvance") }
  }
  @Published private(set) var session: Session?
  @Published private(set) var status = "Inactif"
  @Published private(set) var posterior: [Double] = []
  @Published private(set) var engineState = "idle"
  @Published private(set) var windowStart: Date?
  /// short French reason of the current decision ("nuit courte", "données insuffisantes", …)
  @Published private(set) var decisionReason: String?
  @Published private(set) var anticipationAllowed: Bool?
  @Published private(set) var backupStatus = ""
  @Published private(set) var feedbackPendingSessionID: String?
  @Published private(set) var plan: [String: Any] = [:]
  /// last explicit "Je suis réveillé" / double tap (read by the daily widget as a confirmed awakening)
  @Published private(set) var confirmedAwakeAt: Date? = UserDefaults.standard.object(forKey: "smartWake.confirmedAwakeAt") as? Date
  private var lastReasonCode = ""

  /// anticipation actually requested from the engine
  var effectiveMaxAdvance: Int { mode == .fixed ? 0 : maxAdvanceMin }

  /// the engine keeps the deadline tonight (short night, debt, missing data) and the window
  /// would otherwise already be open: only then is the reason worth showing as a decision
  var deadlineHeld: Bool {
    guard let s = session, anticipationAllowed == false else { return false }
    let adv = s.maxAdvanceMin ?? 30
    return adv > 0 && Date() >= deadline.addingTimeInterval(-TimeInterval(adv * 60))
  }

  /// window start worth showing (nil when it coincides with the deadline: fixed time)
  var displayWindowStart: Date? {
    guard let ws = windowStart, ws < deadline.addingTimeInterval(-30) else { return nil }
    return ws
  }
  let ramp = HapticRampPlayer()

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
    if session != nil { status = "Surveillance du sommeil" }
    ramp.onStrapAlarmsDisabled = { [weak self] in self?.strapAlarmsWereDisabled() }
  }

  /// During a session: the session's own absolute deadline (never re-derived from the clock time,
  /// which would jump to tomorrow once passed). Otherwise the next occurrence of the chosen time.
  var deadline: Date {
    if let s = session { return Date(timeIntervalSince1970: TimeInterval(s.windowEnd)) }
    return GooseBLEClient.nextFutureAlarmDate(from: latest)
  }

  /// When nothing is armed, the picker follows the kind of morning ahead: the time last chosen for
  /// weekdays or for weekends. 15 Nov: Sunday's 10:00 alarm stayed in the picker, so Monday's
  /// recommended bedtime (widget, Sommeil) was computed for a 10:00 wake (~01:30) until the 07:05
  /// alarm was armed at 22:36.
  func alignWakeWithDayType() {
    guard session == nil else { return }
    let cal = Calendar.current
    let next = GooseBLEClient.nextFutureAlarmDate(from: latest)
    guard let m = defaults.object(forKey: Self.wakeKey(next)) as? Int else { return }
    let c = cal.dateComponents([.hour, .minute], from: latest)
    guard (c.hour ?? 0) * 60 + (c.minute ?? 0) != m, let t = cal.date(byAdding: .minute, value: m, to: cal.startOfDay(for: Date())) else { return }
    // the aligned time must still fall on the same kind of morning
    guard Self.wakeKey(GooseBLEClient.nextFutureAlarmDate(from: t)) == Self.wakeKey(next) else { return }
    latest = t
    defaults.set(latest, forKey: Self.latestKey)
  }

  private static func wakeKey(_ morning: Date) -> String {
    Calendar.current.isDateInWeekend(morning) ? "smartWake.wake.weekend" : "smartWake.wake.weekday"
  }

  /// remembers the picker's time for the kind of morning it applies to (next occurrence)
  private func rememberWakeChoice() {
    let c = Calendar.current.dateComponents([.hour, .minute], from: latest)
    defaults.set((c.hour ?? 7) * 60 + (c.minute ?? 0), forKey: Self.wakeKey(GooseBLEClient.nextFutureAlarmDate(from: latest)))
  }

  /// DISABLE_ALARM clears every strap alarm: while a session is armed and its deadline is still
  /// ahead, the deadline alarm must be sent again.
  private func strapAlarmsWereDisabled() {
    guard session != nil else { return }
    deadlineConfirmed = false
    lastDeadlineAttempt = .distantPast
    ble?.record(source: "smart_wake", title: "deadline.resend_needed", body: "strap alarms disabled during an armed session")
  }

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
    if let s = session {
      startHeartbeat()
      restoreBackup(deadline: Date(timeIntervalSince1970: TimeInterval(s.windowEnd)))
    }
    ble.liveSampleSink = { [weak self] date, hr, rr, source in
      self?.ingest(date: date, hr: hr, rr: rr, source: source)
    }
    ble.liveIMUSink = { [weak self] hex, date in
      self?.ingestIMU(hex: hex, date: date)
    }
  }

  // MARK: plan

  func refreshPlan(userChanged: Bool = false) {
    if userChanged { rememberWakeChoice() } else { alignWakeWithDayType() }
    defaults.set(latest, forKey: Self.latestKey)
    let args: [String: Any] = ["database_path": HealthDataStore.defaultDatabasePath(), "latest_wake": Int(deadline.timeIntervalSince1970), "utc_offset_seconds": TimeZone.current.secondsFromGMT(), "max_advance_min": session.map { $0.maxAdvanceMin ?? 30 } ?? effectiveMaxAdvance]
    queue.async { [weak self] in
      let r = (try? GooseRustBridge().request(method: "smart_wake.plan", args: args)) ?? [:]
      Task { @MainActor in self?.plan = r }
    }
  }

  // MARK: arming

  func arm() {
    guard let ble else { return }
    defaults.set(latest, forKey: Self.latestKey)
    rememberWakeChoice()
    let end = deadline
    let now = Date()
    let s = Session(id: "sw-\(Int(now.timeIntervalSince1970))", startTs: Int(now.timeIntervalSince1970), windowEnd: Int(end.timeIntervalSince1970), maxAdvanceMin: effectiveMaxAdvance)
    session = s
    acknowledged = false
    windowStart = nil
    decisionReason = nil
    anticipationAllowed = nil
    lastReasonCode = ""
    defaults.set(try? JSONEncoder().encode(s), forKey: Self.sessionKey)
    deadlineConfirmed = false
    lastDeadlineAttempt = .distantPast
    lastLiveRequest = .distantPast
    ensureStrapSetup(now: now)
    startHeartbeat()
    scheduleBackupNotification(at: end.addingTimeInterval(60))
    let t = end.formatted(date: .omitted, time: .shortened)
    status = "Programmation de l'alarme \(t) sur le bracelet…"
    ble.record(source: "smart_wake", title: "armed", body: "deadline=\(s.windowEnd) max_advance_min=\(effectiveMaxAdvance)")
  }

  func disarm(reason: String = "désactivé") {
    ramp.stop(disableStrap: false)
    session = nil
    windowStart = nil
    decisionReason = nil
    anticipationAllowed = nil
    backupStatus = ""
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

  /// Explicit confirmation: works without any sensor and without waiting for the engine. The
  /// engine is told afterwards so the session is closed in its own journal.
  func acknowledgeAwake() {
    guard let s = session else { ramp.stop(); return }
    acknowledged = true
    let args = engineArgs(s, live: [], imu: [], acknowledged: true)
    queue.async { _ = try? GooseRustBridge().request(method: "smart_wake.update", args: args) }
    ble?.record(source: "smart_wake", title: "awake.confirmed", body: s.id)
    markConfirmedAwake()
    ramp.stop(disableStrap: false)
    feedbackPendingSessionID = s.id
    defaults.set(s.id, forKey: Self.feedbackKey)
    disarm(reason: "réveil confirmé")
    status = "Réveil confirmé. Bonne journée !"
  }

  private func markConfirmedAwake() {
    confirmedAwakeAt = Date()
    defaults.set(confirmedAwakeAt, forKey: "smartWake.confirmedAwakeAt")
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
    // Live HR only from 5 min before the window: while TOGGLE_REALTIME_HR streams, the strap keeps
    // its daytime optical configuration and never switches to its night (SpO2) mode (audit 11 Nov:
    // night mode came 55 min after the stream stopped). Before the window the engine uses R18.
    let opens = windowStart ?? Date(timeIntervalSince1970: TimeInterval(s.windowEnd - 2700))
    let streamPhase = now >= opens.addingTimeInterval(-300) || engineState == "stimulating" || engineState == "probable_awake"
    lock.lock(); let liveAge = now.timeIntervalSince1970 - lastLiveSample; lock.unlock()
    liveStreamOK = liveAge < 15
    if streamPhase, !liveStreamOK, now.timeIntervalSince(lastLiveRequest) >= 30, ble.isFreeForCommands {
      lastLiveRequest = now
      ble.writeSensorStreamCommands([GooseBLEClient.SensorStreamCommandKind(commandNumber: 3, payload: [1], name: "TOGGLE_REALTIME_HR_ON")], requestedStatus: "Smart wake: live HR", updatePhysiologyStatus: false)
    }
    // live IMU (1 frame/s, ~1 s latency) only from 5 min before the window opens: battery
    lock.lock(); let imuAge = now.timeIntervalSince1970 - lastIMU; lock.unlock()
    imuStreamOK = imuAge < 10
    if now >= opens.addingTimeInterval(-300), !imuStreamOK, now.timeIntervalSince(lastIMURequest) >= 30, ble.isFreeForCommands, !ramp.isPlaying {
      lastIMURequest = now
      imuRequested = true
      ble.writeSensorStreamCommands([GooseBLEClient.SensorStreamCommandKind(commandNumber: 106, payload: GooseBLEClient.SensorStreamCommandKind.revisionBoolean(true), name: "TOGGLE_IMU_MODE_ON")], requestedStatus: "Smart wake: live IMU", updatePhysiologyStatus: false)
    }
    let end = Date(timeIntervalSince1970: TimeInterval(s.windowEnd))
    // only while the deadline is still ahead: SET_ALARM_TIME takes a clock time, a passed one
    // would ring tomorrow
    if !deadlineConfirmed, end.timeIntervalSince(now) > 5, now.timeIntervalSince(lastDeadlineAttempt) >= 20, ble.isFreeForCommands, !ramp.isPlaying {
      lastDeadlineAttempt = now
      ble.setWhoopAlarm(at: end, alarmID: 1, pattern: HapticRampPlayer.deadlinePattern)
      DispatchQueue.main.asyncAfter(deadline: .now() + 3) { [weak self, weak ble] in
        guard let self, let ble else { return }
        if ble.lastAlarmResponseSummary.contains("SET_ALARM_TIME"), ble.lastAlarmResponseSummary.contains("SUCCESS") {
          self.deadlineConfirmed = true
          if self.status.hasPrefix("Programmation") { self.status = "Alarme confirmée sur le bracelet · surveillance du sommeil" }
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
    // before the live phase the engine relies on R18: keep it at most ~5 min old
    let interval: TimeInterval = engineState == "stimulating" ? 45 : nearWindow ? 120 : 300
    if let ble, ble.canStartPeriodicSync, !ramp.isPlaying, now.timeIntervalSince(lastSyncRequest) >= interval {
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
    let args = engineArgs(s, live: live, imu: imu, acknowledged: acknowledged)
    queue.async { [weak self] in
      let result = try? GooseRustBridge().request(method: "smart_wake.update", args: args)
      Task { @MainActor in self?.apply(result) }
    }
  }

  private func engineArgs(_ s: Session, live: [[Any]], imu: [[Any]], acknowledged: Bool) -> [String: Any] {
    var session: [String: Any] = ["id": s.id, "start_ts": s.startTs, "window_start": 0, "window_end": s.windowEnd, "utc_offset_seconds": TimeZone.current.secondsFromGMT()]
    if let adv = s.maxAdvanceMin { session["max_advance_min"] = adv }
    return [
      "database_path": HealthDataStore.defaultDatabasePath(),
      "session": session,
      "live": live,
      "imu": imu,
      "acknowledged": acknowledged,
    ]
  }

  private func apply(_ r: [String: Any]?) {
    running = false
    guard let r, let s = session else {
      // engine hiccup: retried on the next tick; the strap deadline alarm does not depend on it
      return
    }
    engineState = r["state"] as? String ?? engineState
    if let w = r["window"] as? [String: Any], let ws = w["start"] as? Int {
      windowStart = Date(timeIntervalSince1970: TimeInterval(ws))
    }
    if let epoch = r["epoch"] as? [String: Any] {
      posterior = epoch["posterior"] as? [Double] ?? posterior
    }
    anticipationAllowed = (r["anticipation"] as? [String: Any])?["allowed"] as? Bool
    if let reason = r["decision_reason"] as? [String: Any], let code = reason["code"] as? String {
      decisionReason = reason["label"] as? String
      // the engine journals every decision; the app log keeps each change of reason
      if code != lastReasonCode {
        lastReasonCode = code
        ble?.record(source: "smart_wake", title: "decision", body: "\(code) state=\(engineState) action=\(r["action"] ?? "none")")
      }
    }
    let why = r["why"] as? String ?? ""
    switch r["action"] as? String {
    case "stimulate":
      let level = r["level"] as? Int ?? 1
      ramp.play(level: level)
      status = "Réveil en cours (niveau \(level)) : \(why)"
      ble?.record(source: "smart_wake", title: "stimulate", body: "level=\(level) why=\(why)")
    case "probable_awake":
      // strong but unconfirmed: pause the ramp, keep the deadline alarm on the strap
      ramp.stop(disableStrap: false)
      status = "Éveil probable (\(why)). Confirme pour couper l'alarme de \(deadline.formatted(date: .omitted, time: .shortened))."
      ble?.record(source: "smart_wake", title: "probable_awake", body: "\(s.id) \(why)")
    case "awake":
      markConfirmedAwake()
      ramp.stop()
      ble?.record(source: "smart_wake", title: "awake", body: "\(s.id) \(why)")
      feedbackPendingSessionID = s.id
      defaults.set(s.id, forKey: Self.feedbackKey)
      disarm(reason: "réveil confirmé")
      status = "Réveil confirmé (\(why)). Bonne journée !"
    case "timeout":
      ramp.stop(disableStrap: false)
      ble?.record(source: "smart_wake", title: "timeout", body: s.id)
      feedbackPendingSessionID = s.id
      defaults.set(s.id, forKey: Self.feedbackKey)
      disarm(reason: "terminé sans confirmation")
    case "deadline":
      status = "Heure limite : alarme du bracelet"
    default:
      switch engineState {
      case "probable_awake":
        status = "Éveil probable. L'alarme de \(deadline.formatted(date: .omitted, time: .shortened)) reste programmée tant que tu n'as pas confirmé."
      case "armed":
        let reason = decisionReason.map { " · \($0)" } ?? ""
        let adv = s.maxAdvanceMin ?? 30
        let earliest = deadline.addingTimeInterval(-TimeInterval(adv * 60))
        if adv > 0, Date() >= earliest, anticipationAllowed == false {
          status = "Heure limite conservée\(reason)"
        } else if let ws = displayWindowStart, Date() >= ws {
          status = "Fenêtre ouverte\(reason)"
        } else {
          status = "Surveillance du sommeil"
        }
      default:
        break
      }
    }
  }

  /// After a relaunch: the notification scheduled at arming is still pending, or is scheduled again.
  private func restoreBackup(deadline: Date) {
    guard deadline > Date() else { return }
    UNUserNotificationCenter.current().getPendingNotificationRequests { [weak self] reqs in
      let pending = reqs.contains { $0.identifier == Self.notificationID }
      Task { @MainActor in
        guard let self, self.session != nil else { return }
        if pending {
          self.backupStatus = "Secours iPhone programmé à \(deadline.addingTimeInterval(60).formatted(date: .omitted, time: .shortened))"
        } else {
          self.scheduleBackupNotification(at: deadline.addingTimeInterval(60))
        }
      }
    }
  }

  private func scheduleBackupNotification(at date: Date) {
    let center = UNUserNotificationCenter.current()
    backupStatus = "Secours iPhone : programmation…"
    center.requestAuthorization(options: [.alert, .sound]) { [weak self] granted, _ in
      guard granted else {
        Task { @MainActor in self?.backupStatus = "Secours iPhone non autorisé (notifications désactivées)" }
        return
      }
      let content = UNMutableNotificationContent()
      content.title = "Réveil"
      content.body = "Heure limite de ton réveil."
      content.sound = .default
      let comps = Calendar.current.dateComponents([.year, .month, .day, .hour, .minute], from: date)
      center.add(UNNotificationRequest(identifier: Self.notificationID, content: content, trigger: UNCalendarNotificationTrigger(dateMatching: comps, repeats: false))) { error in
        Task { @MainActor in
          self?.backupStatus = error == nil ? "Secours iPhone programmé à \(date.formatted(date: .omitted, time: .shortened)) (son de notification, non garanti en mode silencieux)" : "Secours iPhone non programmé"
        }
      }
    }
  }
}

// MARK: - Alarm screen (Forme)

/// Smart wake screen. Answers three questions in order: when will I be woken, is it really
/// programmed, what can I change. Model details stay folded in "Détails techniques".
struct AlarmView: View {
  @ObservedObject var controller: SmartWakeController
  @ObservedObject private var ble: GooseBLEClient
  @ObservedObject private var ramp: HapticRampPlayer
  @State private var editingTime = false

  init(controller: SmartWakeController, ble: GooseBLEClient) {
    self.controller = controller
    _ramp = ObservedObject(wrappedValue: controller.ramp)
    _ble = ObservedObject(wrappedValue: ble)
  }

  private var armed: Bool { controller.session != nil }
  private var waking: Bool { controller.engineState == "stimulating" || controller.engineState == "probable_awake" || (armed && ramp.isPlaying) }
  private var adv: Int { controller.session.map { $0.maxAdvanceMin ?? 30 } ?? controller.effectiveMaxAdvance }

  var body: some View {
    FormeScroll {
      DetailHeader(title: "Réveil")
      hero.padding(.top, 24)
      if waking { wakeCard.padding(.top, 12) }
      if controller.feedbackPendingSessionID != nil, !armed { feedbackCard.padding(.top, 12) }
      primaryButton.padding(.top, 16)
      if armed { tonightStatus } else { settings }
      planCard
      if armed { TechDetails(subtitle: "Stades estimés, fenêtre et flux du bracelet", rows: techRows).padding(.top, 32) }
    }
    .formeScreen()
    .onAppear { controller.refreshPlan() }
    .onChange(of: controller.latest) { _, _ in controller.refreshPlan(userChanged: true) }
    .onChange(of: controller.mode) { _, _ in controller.refreshPlan() }
    .onChange(of: controller.maxAdvanceMin) { _, _ in controller.refreshPlan() }
    .refreshable { controller.refreshPlan() }
    .sheet(isPresented: $editingTime) { timeSheet }
  }

  // MARK: hero

  private var hero: some View {
    AuraCard(family: .sleep, radius: 44, padding: 24) {
      HStack {
        Text(adv == 0 ? "Heure fixe" : "Fenêtre flexible")
          .font(Forme.sans(20, .regular, relativeTo: .title3)).tracking(-0.2)
        Spacer()
        VeilPill(text: armed ? "Armé" : "Désactivé", ink: armed ? Forme.Family.sleep.pillInk : Forme.onBright2)
      }
      Button { if !armed { editingTime = true } } label: {
        VStack(alignment: .leading, spacing: 2) {
          Text(adv == 0 ? "Réveil à" : "Au plus tard").font(Forme.sans(13, .semibold, relativeTo: .footnote)).foregroundStyle(Forme.onBright2)
          DotNumber(value: FF.clock(armed ? controller.deadline : controller.latest), size: 84)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .contentShape(Rectangle())
      }
      .buttonStyle(.plain)
      .allowsHitTesting(!armed)
      .padding(.top, 18)
      .accessibilityLabel("Heure de réveil \(FF.clock(armed ? controller.deadline : controller.latest))")
      .accessibilityHint(armed ? "" : "Modifier l'heure")
      Text(windowSentence)
        .font(Forme.sans(15, .regular, relativeTo: .body)).foregroundStyle(Forme.hex(0x1c2426))
        .fixedSize(horizontal: false, vertical: true)
        .padding(.top, 10)
      VeilPanel {
        HStack(spacing: 10) {
          Image(systemName: armed ? (controller.deadlineConfirmed ? "checkmark.seal.fill" : "hourglass") : "hand.tap")
            .font(.system(size: 15, weight: .semibold))
          Text(armed ? controller.status : "Touche l'heure pour la modifier")
            .font(Forme.sans(13, .semibold, relativeTo: .footnote))
            .fixedSize(horizontal: false, vertical: true)
        }
        .foregroundStyle(Forme.onBright)
      }
      .padding(.top, 16)
    }
    .accessibilityElement(children: .contain)
  }

  private var windowSentence: String {
    let end = FF.clock(controller.deadline)
    if adv == 0 { return "Réveil à \(end) précises, sans anticipation." }
    if controller.deadlineHeld, let r = controller.decisionReason {
      return "Réveil à \(end) : \(r)."
    }
    if let ws = controller.displayWindowStart { return "Réveil possible entre \(FF.clock(ws)) et \(end)." }
    let earliest = controller.deadline.addingTimeInterval(-TimeInterval(adv * 60))
    return "Réveil possible entre \(FF.clock(earliest)) et \(end), si ton besoin de sommeil est couvert."
  }

  private var timeSheet: some View {
    VStack(spacing: 20) {
      Text("Heure de réveil").font(Forme.sans(17, .semibold, relativeTo: .headline)).foregroundStyle(Forme.ink)
      DatePicker("Heure de réveil", selection: $controller.latest, displayedComponents: .hourAndMinute)
        .datePickerStyle(.wheel).labelsHidden()
      Button { editingTime = false } label: {
        Text("Valider").font(Forme.sans(17, .semibold, relativeTo: .headline)).foregroundStyle(Forme.canvas)
          .frame(maxWidth: .infinity, minHeight: 56).background(Forme.ink, in: Capsule())
      }
      .buttonStyle(.plain)
    }
    .padding(24)
    .presentationDetents([.medium])
    .background(FormeGround())
  }

  // MARK: primary action

  private var primaryButton: some View {
    Button {
      armed ? controller.disarm() : controller.arm()
    } label: {
      Text(armed ? "Désactiver le réveil" : "Programmer le réveil")
        .font(Forme.sans(17, .semibold, relativeTo: .headline))
        .foregroundStyle(armed ? Forme.ink : Forme.canvas)
        .frame(maxWidth: .infinity, minHeight: 56)
        .background(armed ? Forme.sunken : Forme.ink, in: Capsule())
    }
    .buttonStyle(FormePressStyle())
  }

  // MARK: waking up

  private var wakeCard: some View {
    AuraCard(family: .calm, radius: 36, padding: 22) {
      Text(controller.engineState == "probable_awake" ? "Éveil probable" : "C'est l'heure")
        .font(Forme.sans(24, .regular, relativeTo: .title2)).tracking(-0.24)
      Text(controller.engineState == "probable_awake"
           ? "L'alarme de \(FF.clock(controller.deadline)) reste programmée tant que tu n'as pas confirmé."
           : "Vibration en cours · \(ramp.percent) %. Un double tap sur le bracelet confirme aussi.")
        .font(Forme.sans(15, .regular, relativeTo: .body)).foregroundStyle(Forme.hex(0x1c2426))
        .fixedSize(horizontal: false, vertical: true)
        .padding(.top, 6)
      Button { controller.acknowledgeAwake() } label: {
        Text("Je suis réveillé").font(Forme.sans(19, .bold, relativeTo: .title3)).foregroundStyle(.white)
          .frame(maxWidth: .infinity, minHeight: 60).background(Forme.onBright, in: Capsule())
      }
      .buttonStyle(FormePressStyle())
      .accessibilityHint("Arrête la vibration et l'alarme de l'heure limite")
      .padding(.top, 18)
    }
  }

  // MARK: settings (before arming)

  private var settings: some View {
    Group {
      SectionTitle(title: "Réglages", subtitle: "Modifiables tant que le réveil n'est pas programmé")
      FrostCard(radius: 36, padding: 20, spacing: 16) {
        FormeSegments(options: SmartWakeController.Mode.allCases.map { ($0.title, $0) }, selection: $controller.mode)
        if controller.mode == .flexible {
          VStack(alignment: .leading, spacing: 10) {
            Text("Anticipation maximale").font(Forme.sans(13, .semibold, relativeTo: .footnote)).foregroundStyle(Forme.ink2)
            FormeSegments(options: SmartWakeController.advanceChoices.map { ("\($0) min", $0) }, selection: $controller.maxAdvanceMin)
          }
          Text("Le sommeil total passe d'abord. Si un réveil plus tôt n'apporte pas de bénéfice clair — nuit courte, dette de sommeil, données insuffisantes — tu es réveillé à l'heure choisie.")
            .font(Forme.sans(12, .medium, relativeTo: .caption)).foregroundStyle(Forme.muted)
            .fixedSize(horizontal: false, vertical: true)
        } else {
          Text("Réveil à l'heure choisie, sans chercher de moment plus favorable.")
            .font(Forme.sans(12, .medium, relativeTo: .caption)).foregroundStyle(Forme.muted)
        }
      }
      .padding(.top, 12)
    }
  }

  // MARK: tonight (armed)

  private var tonightStatus: some View {
    Group {
      SectionTitle(title: "Cette nuit", subtitle: "Ce qui est réellement programmé")
      FrostCard(radius: 36, padding: 20, spacing: 0) {
        FormeRow(label: "Alarme du bracelet", sub: "Sonne même si le téléphone est éteint", value: controller.deadlineConfirmed ? "Confirmée" : "Envoi en cours", tone: controller.deadlineConfirmed ? Forme.posInk : Forme.ink2)
        FormeDivider()
        FormeRow(label: "Secours iPhone", sub: backupSub, value: backupValue, tone: backupValue == "Programmé" ? Forme.posInk : Forme.ink2)
        FormeDivider()
        FormeRow(label: "Suivi du sommeil", sub: controller.liveStreamOK ? "Fréquence cardiaque en direct" : "Direct activé 5 min avant la fenêtre", value: controller.liveStreamOK ? "Actif" : "Historique", tone: controller.liveStreamOK ? Forme.posInk : Forme.ink2)
        if let r = controller.decisionReason, r != "avant la fenêtre" {
          FormeDivider()
          FormeRow(label: "Décision", sub: controller.deadlineHeld ? "Heure limite conservée" : nil, value: String(r.prefix(1)).uppercased() + String(r.dropFirst()))
        }
      }
      .padding(.top, 12)
    }
  }

  private var backupValue: String {
    let s = controller.backupStatus
    if s.hasPrefix("Secours iPhone programmé") { return "Programmé" }
    if s.contains("non autorisé") { return "Non autorisé" }
    if s.contains("non programmé") { return "Échec" }
    return s.isEmpty ? "—" : "En cours"
  }

  private var backupSub: String {
    backupValue == "Non autorisé" ? "Active les notifications de l'app" : "Notification 1 min après l'heure limite"
  }

  // MARK: tonight's plan

  private func num(_ key: String) -> Double? {
    let v = controller.plan[key]
    return (v as? Double) ?? (v as? Int).map(Double.init)
  }

  @ViewBuilder private var planCard: some View {
    SectionTitle(title: "Ce soir", subtitle: "Pour dormir ce dont tu as besoin")
    FrostCard(radius: 36, padding: 20, spacing: 14) {
      if let rec = num("recommended_sleep_min"), let bed = num("bedtime") {
        HStack(alignment: .top, spacing: 8) {
          MiniStat(label: "Décrocher", value: FF.clock(num("lights_out_advice")))
          MiniStat(label: "Au lit à", value: FF.clock(bed))
          MiniStat(label: "Sommeil", value: FF.dur(rec))
        }
        if let lat = num("latency_min") {
          Text("Tu mets en général \(Int(lat)) min à t'endormir : te coucher à \(FF.clock(bed)) couvre ton besoin avant \(FF.clock(controller.deadline)).")
            .font(Forme.sans(13, .medium, relativeTo: .footnote)).foregroundStyle(Forme.ink2)
            .fixedSize(horizontal: false, vertical: true)
        }
        if let c = controller.plan["components"] as? [String: Any] {
          planRows(planParts(c))
        }
        if let note = controller.plan["circadian_note"] as? String {
          Text(note).font(Forme.sans(12, .semibold, relativeTo: .caption)).foregroundStyle(Forme.strainInk)
            .fixedSize(horizontal: false, vertical: true)
        }
        if let line = bedNowLine {
          Text(line).font(Forme.sans(12, .medium, relativeTo: .caption)).foregroundStyle(Forme.muted)
        }
      } else {
        Text("Calcul du plan…").font(Forme.sans(15, .regular, relativeTo: .body)).foregroundStyle(Forme.ink2)
      }
    }
    .padding(.top, 12)
  }

  private func planParts(_ c: [String: Any]) -> [(String, Double)] {
    let keys: [(String, String)] = [("Base personnelle", "base_min"), ("Activité récente", "activity_min"), ("Dette de sommeil", "debt_min"), ("Rattrapage du profond", "deep_min")]
    var out: [(String, Double)] = []
    for (i, k) in keys.enumerated() {
      let v = (c[k.1] as? Double) ?? (c[k.1] as? Int).map(Double.init) ?? 0
      if i == 0 || v >= 1 { out.append((k.0, v)) }
    }
    return out
  }

  private func planRows(_ parts: [(String, Double)]) -> some View {
    VStack(spacing: 0) {
      ForEach(Array(parts.enumerated()), id: \.offset) { i, p in
        if i > 0 { FormeDivider() }
        FormeRow(label: p.0, value: i == 0 ? FF.dur(p.1) : "+\(FF.dur(p.1))")
      }
    }
  }

  private var bedNowLine: String? {
    guard let now = controller.plan["if_bed_now"] as? [String: Any] else { return nil }
    let n: (String) -> Double? = { k in (now[k] as? Double) ?? (now[k] as? Int).map(Double.init) }
    guard let possible = n("sleep_possible_min") else { return nil }
    let deficit = n("deficit_min") ?? 0
    // only meaningful in the evening, not in the middle of the day
    if deficit <= 0, let rec = num("recommended_sleep_min"), possible > rec + 120 { return nil }
    return deficit > 0 ? "Couché maintenant : \(FF.dur(possible)) possibles, \(FF.dur(deficit)) de moins que conseillé." : "Couché maintenant : \(FF.dur(possible)) possibles, ton besoin est couvert."
  }

  // MARK: feedback

  private var feedbackCard: some View {
    FrostCard(radius: 36, padding: 20, spacing: 14) {
      Text("Comment t'es-tu senti au réveil ?").font(Forme.sans(17, .semibold, relativeTo: .headline)).foregroundStyle(Forme.ink)
      HStack(spacing: 8) {
        ForEach(1...5, id: \.self) { r in
          Button { controller.submitFeedback(rating: r) } label: {
            Text("\(r)").font(Forme.sans(17, .bold, relativeTo: .headline)).foregroundStyle(Forme.ink)
              .frame(maxWidth: .infinity, minHeight: 48).background(Forme.inset, in: Capsule())
          }
          .buttonStyle(FormePressStyle())
          .accessibilityLabel("Note \(r) sur 5")
        }
      }
      HStack {
        Text("Groggy")
        Spacer()
        Text("En forme")
      }
      .font(Forme.sans(12, .medium, relativeTo: .caption)).foregroundStyle(Forme.muted)
    }
  }

  // MARK: technical details

  private var techRows: [(String, String, String)] {
    let p = controller.posterior
    let pct: (Int) -> String = { i in p.count == 4 ? "\(Int((p[i] * 100).rounded())) %" : "—" }
    return [
      ("Éveil", "Probabilité estimée sur les 30 dernières secondes", pct(0)),
      ("Sommeil léger", "", pct(1)),
      ("Sommeil profond", "", pct(2)),
      ("Sommeil paradoxal", "", pct(3)),
      ("Ouverture de la fenêtre", "Endormissement estimé + sommeil recommandé", controller.displayWindowStart.map { FF.clock($0) } ?? "Heure limite"),
      ("Anticipation maximale", "", adv == 0 ? "Aucune" : "\(adv) min"),
      ("Mouvement en direct", "Accéléromètre, activé 5 min avant la fenêtre", controller.imuStreamOK ? "Actif" : "Inactif"),
      ("Réponse du bracelet", "Dernière commande d'alarme", ble.lastAlarmResponseSummary.isEmpty ? "—" : ble.lastAlarmResponseSummary),
    ]
  }
}

/// Segmented choice on glass, same look as the period picker.
struct FormeSegments<T: Hashable>: View {
  let options: [(String, T)]
  @Binding var selection: T
  var body: some View {
    HStack(spacing: 0) {
      ForEach(Array(options.enumerated()), id: \.offset) { _, o in
        Button { withAnimation(.snappy(duration: 0.25)) { selection = o.1 } } label: {
          Text(o.0).font(Forme.sans(13, .bold, relativeTo: .footnote)).foregroundStyle(Forme.ink)
            .lineLimit(1).minimumScaleFactor(0.8)
            .frame(maxWidth: .infinity, minHeight: 40)
            .background(selection == o.1 ? Forme.selection : .clear, in: Capsule())
        }
        .buttonStyle(.plain)
        .accessibilityAddTraits(selection == o.1 ? .isSelected : [])
      }
    }
    .padding(4)
    .glass(Capsule())
  }
}

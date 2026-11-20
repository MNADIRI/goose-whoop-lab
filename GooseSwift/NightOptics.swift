import Combine
import Foundation
import SwiftUI

/// Night optical windows (Rust/core/src/night_optics.rs), a 4-night test agreed by the user on
/// 17 Nov (cmd 153) and 19 Nov (with ENABLE_OPTICAL_DATA 107 / live optical mode 108): on nights
/// 1 and 3, the full optical frame is recorded for 70 s around each of the strap's own SpO2
/// measurements (30 s every 20 min during its sleep), plus a control window every third cycle;
/// nights 2 and 4 are the battery reference. Frames reach the phone with the normal history sync
/// (and live in mode B).
///
/// Safety: the end of the open window is persisted; every tick (and every launch) sends OFF once
/// it has passed, until the strap acknowledges it. Optical frames arriving long after the last
/// OFF also trigger an OFF. After the 4 nights nothing is switched on any more.
@MainActor
final class NightOpticsController: ObservableObject {
  static let shared = NightOpticsController()

  struct Window: Codable {
    var night: String
    var kind: String
    var start: Int
    var end: Int
    var on_at: Int?
    var off_at: Int?
    var on_result: Int?
    /// "A": ENABLE_OPTICAL_DATA + persistent R20; "B": + live optical mode
    var mode: String?
  }

  struct Battery: Codable {
    var ts: Int
    var pct: Double
  }

  struct Night: Codable {
    var night: String
    var on: Bool
  }

  @Published private(set) var report: [String: Any] = [:]
  @Published private(set) var status = ""

  private weak var ble: GooseBLEClient?
  private var bag = Set<AnyCancellable>()
  private let queue = DispatchQueue(label: "night-optics", qos: .utility)
  private let defaults = UserDefaults.standard
  private var windows: [Window] = []
  private var battery: [Battery] = []
  private(set) var nights: [Night] = []
  private var next: [[String: Any]] = []
  private var lastPlanAt = Date.distantPast
  private var r20LastTs: Int?
  /// a command waiting for the strap or its answer
  private var pending: (on: Bool, at: Date)?
  /// OFF sent without an answer: after 5, the window is closed anyway (the frame check above
  /// sends a new OFF if the strap kept recording)
  private var offAttempts = 0
  /// mode of a test window
  private var modeOverride: String?

  private enum Key {
    static let windows = "nightOptics.windows"
    static let battery = "nightOptics.battery"
    static let nights = "nightOptics.nights"
    static let onUntil = "nightOptics.r20OnUntil"
    static let since = "nightOptics.since"
    static let mode = "nightOptics.mode"
  }

  /// A until the report shows that mode-A windows bring no frame, then B (live optical mode too)
  private var mode: String {
    get { defaults.string(forKey: Key.mode) ?? "A" }
    set { defaults.set(newValue, forKey: Key.mode) }
  }

  /// end of the window the strap is recording (persisted: an OFF is owed until acknowledged)
  private var onUntil: Int? {
    get { defaults.object(forKey: Key.onUntil) as? Int }
    set { defaults.set(newValue, forKey: Key.onUntil) }
  }

  func attach(ble: GooseBLEClient) {
    self.ble = ble
    load()
    if nights.isEmpty {
      // 4 nights from tonight: with, without, with, without
      let first = Self.nightKey(Int(Date().timeIntervalSince1970))
      nights = (0..<4).map { Night(night: Self.addDays(first, $0), on: $0 % 2 == 0) }
      defaults.set(Int(Date().timeIntervalSince1970), forKey: Key.since)
      save()
    }
    // 10 s: a window opens 20 s before the strap's 30 s SpO2 measurement
    Timer.publish(every: 10, tolerance: 2, on: .main, in: .common).autoconnect().sink { [weak self] _ in self?.tick() }.store(in: &bag)
    Timer.publish(every: 20 * 60, tolerance: 60, on: .main, in: .common).autoconnect().sink { [weak self] _ in self?.refreshReport() }.store(in: &bag)
    tick()
    refreshReport()
  }

  // MARK: schedule

  private func tick() {
    let now = Int(Date().timeIntervalSince1970)
    if let p = pending, Date().timeIntervalSince(p.at) > 25 { pending = nil }
    // an OFF is owed
    if let until = onUntil, now >= until {
      if offAttempts >= 5, pending == nil {
        closeWindow(at: now)
      } else {
        send(on: false)
      }
      return
    }
    let night = nights.first { $0.night == Self.nightKey(now) }
    let planEvery: TimeInterval = night?.on == true ? 60 : 600
    if Date().timeIntervalSince(lastPlanAt) > planEvery { plan() }
    // frames still arriving well after the last OFF: the strap kept recording
    let lastOff = windows.compactMap(\.off_at).max() ?? (defaults.object(forKey: Key.since) as? Int ?? now)
    if onUntil == nil, let r = r20LastTs, r > lastOff + 300, pending == nil {
      ble?.record(level: .warn, source: "night_optics", title: "r20.still_recording", body: "last frame \(r), last off \(lastOff)")
      onUntil = now
      send(on: false)
      return
    }
    guard night?.on == true, onUntil == nil, pending == nil else { return }
    // open on time: up to 20 s late still covers the strap's measurement (it starts 20 s in)
    guard let w = next.first(where: { ($0["start"] as? Int ?? 0) - 10 <= now && now < ($0["start"] as? Int ?? 0) + 20 }),
          let start = w["start"] as? Int, let end = w["end"] as? Int, let kind = w["kind"] as? String,
          end - now >= 30, !windows.contains(where: { $0.start == start }) else { return }
    windows.append(Window(night: Self.nightKey(now), kind: kind, start: start, end: end, mode: mode))
    onUntil = end
    save()
    send(on: true)
  }

  private func send(on: Bool) {
    guard let ble, pending == nil else { return }
    pending = (on, Date())
    ble.whenFreeForCommands(timeout: 20) { [weak self] ble in
      guard let self else { return }
      typealias K = GooseBLEClient.SensorStreamCommandKind
      // the record flag alone records nothing (17 Nov): the optical feed (107) goes with it, and the
      // live optical mode (108) in mode B; OFF closes all three, recording flag first
      let live = (self.modeOverride ?? self.mode) == "B"
      let cmds: [K] = on
        ? [K(commandNumber: 107, payload: K.revisionBoolean(true), name: "ENABLE_OPTICAL_DATA_ON")]
          + (live ? [K(commandNumber: 108, payload: K.revisionBoolean(true), name: "TOGGLE_OPTICAL_MODE_ON")] : [])
          + [K(commandNumber: 153, payload: K.revisionBoolean(true), name: "TOGGLE_PERSISTENT_R20_ON")]
        : [K(commandNumber: 153, payload: K.revisionBoolean(false), name: "TOGGLE_PERSISTENT_R20_OFF"),
           K(commandNumber: 108, payload: K.revisionBoolean(false), name: "TOGGLE_OPTICAL_MODE_OFF"),
           K(commandNumber: 107, payload: K.revisionBoolean(false), name: "ENABLE_OPTICAL_DATA_OFF")]
      self.pending = (on, Date())
      if on, let i = self.windows.indices.last, self.windows[i].on_at == nil {
        self.windows[i].on_at = Int(Date().timeIntervalSince1970)
        self.save()
      }
      if !on { self.offAttempts += 1 }
      ble.writeSensorStreamCommands(cmds, requestedStatus: "Fenêtre optique", updatePhysiologyStatus: false)
      ble.record(source: "night_optics", title: on ? "r20.on" : "r20.off", body: "mode=\(self.mode) until=\(self.onUntil ?? 0)")
    }
  }

  /// development: one window now, through the same commands and safety as the night ones
  func testWindow(seconds: Int, mode testMode: String) {
    guard onUntil == nil, pending == nil else { return }
    let now = Int(Date().timeIntervalSince1970)
    modeOverride = testMode
    windows.append(Window(night: Self.nightKey(now), kind: "test", start: now, end: now + seconds, mode: testMode))
    onUntil = now + seconds
    save()
    send(on: true)
  }

  /// strap answer to cmd 153 (GooseBLEClient+HistoricalHandlers)
  func acknowledged(result: Int) {
    guard let p = pending else { return }
    pending = nil
    let now = Int(Date().timeIntervalSince1970)
    if p.on {
      if let i = windows.indices.last, windows[i].on_result == nil { windows[i].on_result = result }
      if result != 1 { onUntil = now }
      save()
    } else if result == 1 {
      closeWindow(at: now)
    }
    status = p.on ? "on \(result)" : "off \(result)"
  }

  private func closeWindow(at now: Int) {
    if let i = windows.indices.last, windows[i].on_at != nil, windows[i].off_at == nil { windows[i].off_at = now }
    onUntil = nil
    offAttempts = 0
    modeOverride = nil
    save()
    // the window's frames come with the next syncs: report (and escalation) a few minutes later
    DispatchQueue.main.asyncAfter(deadline: .now() + 8 * 60) { [weak self] in self?.refreshReport() }
  }

  private func plan() {
    lastPlanAt = Date()
    let args: [String: Any] = ["database_path": HealthDataStore.defaultDatabasePath(), "utc_offset_seconds": TimeZone.current.secondsFromGMT()]
    queue.async { [weak self] in
      let r = try? GooseRustBridge().request(method: "night_optics.plan", args: args)
      Task { @MainActor in
        guard let self, let r else { return }
        self.next = r["next"] as? [[String: Any]] ?? []
        self.r20LastTs = r["r20_last_ts"] as? Int
        self.status = r["reason"] as? String ?? ""
      }
    }
  }

  // MARK: battery

  /// every strap battery reading during the test (GooseBLEClient.applyBatteryLevel)
  func battery(percent: Int, at: Date) {
    guard let last = nights.last, Self.nightKey(Int(at.timeIntervalSince1970)) <= Self.addDays(last.night, 1) else { return }
    let ts = Int(at.timeIntervalSince1970)
    if let b = battery.last, b.pct == Double(percent), ts - b.ts < 1800 { return }
    battery.append(Battery(ts: ts, pct: Double(percent)))
    if battery.count > 3000 { battery.removeFirst(battery.count - 3000) }
    save()
  }

  // MARK: report

  func refreshReport() {
    guard !nights.isEmpty else { return }
    let args: [String: Any] = [
      "database_path": HealthDataStore.defaultDatabasePath(),
      "utc_offset_seconds": TimeZone.current.secondsFromGMT(),
      "windows": Self.plain(windows), "battery": Self.plain(battery), "nights": Self.plain(nights),
    ]
    queue.async { [weak self] in
      let r = try? GooseRustBridge().request(method: "night_optics.report", args: args)
      Task { @MainActor in
        guard let self, let r else { return }
        self.report = r
        if r["escalate"] as? Bool == true, self.mode == "A" {
          self.mode = "B"
          self.ble?.record(source: "night_optics", title: "mode.escalated", body: "A brought no frame: live optical mode added")
        }
      }
    }
  }

  private static func plain<T: Encodable>(_ v: T) -> Any {
    (try? JSONSerialization.jsonObject(with: JSONEncoder().encode(v))) ?? []
  }

  // MARK: storage

  private func load() {
    let dec = JSONDecoder()
    if let d = defaults.data(forKey: Key.windows), let v = try? dec.decode([Window].self, from: d) { windows = v }
    if let d = defaults.data(forKey: Key.battery), let v = try? dec.decode([Battery].self, from: d) { battery = v }
    if let d = defaults.data(forKey: Key.nights), let v = try? dec.decode([Night].self, from: d) { nights = v }
  }

  private func save() {
    let enc = JSONEncoder()
    defaults.set(try? enc.encode(windows), forKey: Key.windows)
    defaults.set(try? enc.encode(battery), forKey: Key.battery)
    defaults.set(try? enc.encode(nights), forKey: Key.nights)
  }

  /// date of the evening: local date 12 h earlier (same rule as night_optics::night_key)
  static func nightKey(_ ts: Int) -> String {
    let f = DateFormatter()
    f.locale = Locale(identifier: "en_US_POSIX")
    f.timeZone = .current
    f.dateFormat = "yyyy-MM-dd"
    return f.string(from: Date(timeIntervalSince1970: TimeInterval(ts - 12 * 3600)))
  }

  static func nightDate(_ key: String) -> Date {
    let f = DateFormatter()
    f.locale = Locale(identifier: "en_US_POSIX")
    f.timeZone = .current
    f.dateFormat = "yyyy-MM-dd HH"
    return f.date(from: key + " 12") ?? Date()
  }

  static func addDays(_ key: String, _ n: Int) -> String {
    let f = DateFormatter()
    f.locale = Locale(identifier: "en_US_POSIX")
    f.timeZone = TimeZone(identifier: "UTC")
    f.dateFormat = "yyyy-MM-dd"
    guard let d = f.date(from: key) else { return key }
    return f.string(from: d.addingTimeInterval(TimeInterval(n * 86_400)))
  }
}

/// Respiration et pouls: the 4-night test, night by night.
struct FormeNightOpticsCard: View {
  @ObservedObject var store: NightOpticsController

  var body: some View {
    let r = J(store.report)
    if !store.nights.isEmpty {
      SectionTitle(title: "Fenêtres optiques de nuit", subtitle: "70 s autour de chaque mesure SpO₂ du bracelet (30 s toutes les 20 min), une nuit sur deux, pour la paire rouge + infrarouge")
      FrostCard(radius: 32, padding: 18, spacing: 8) {
        let byNight = Dictionary(r["nights"].arr.compactMap { n in n["night"].s.map { ($0, n) } }, uniquingKeysWith: { a, _ in a })
        ForEach(store.nights, id: \.night) { n in
          let x = byNight[n.night]
          FormeRow(label: FF.day(NightOpticsController.nightDate(n.night), "EEE d MMM") + (n.on ? " · avec fenêtres" : " · référence"),
                   sub: sub(x, on: n.on),
                   value: x?["battery_pct_per_h"].d.map { "\(FF.num($0, 2)) %/h" } ?? "—")
        }
        ForEach(Array(r["conclusions"].arr.enumerated()), id: \.offset) { i, c in
          if i == 0 { FormeDivider() }
          Text(c.s ?? "").font(Forme.sans(13, .regular, relativeTo: .footnote)).foregroundStyle(Forme.ink2).fixedSize(horizontal: false, vertical: true)
        }
      }
      .padding(.top, 12)
    }
  }

  private func sub(_ x: J?, on: Bool) -> String {
    guard let x else { return on ? "à venir" : "batterie seule" }
    var parts: [String] = []
    if on {
      parts.append("\(x["windows"].i ?? 0) fenêtres · \((x["r20_seconds"].i ?? 0) / 60) min enregistrées")
      if (x["r20_seconds"].i ?? 0) > 0 {
        parts.append((x["spo2_seconds"].i ?? 0) > 0 ? "rouge + infrarouge présents" : "rouge + infrarouge absents")
      } else if (x["windows"].i ?? 0) > 0 {
        parts.append("aucune trame reçue")
      }
    }
    if let h = x["battery_hours"].d { parts.append("batterie sur \(FF.num(h, 1)) h") }
    return parts.isEmpty ? "en cours" : parts.joined(separator: " · ")
  }
}

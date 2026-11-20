import CoreBluetooth
import Foundation

/// Recovery from a strap whose history transfer is stuck.
///
/// 13 Nov 00:17 -> 09:34 and 19 Nov 23:42 -> 20 Nov 10:34: GET_DATA_RANGE / SEND_HISTORICAL_DATA are
/// answered (or time out) but the strap never sends a page again — its read pointer stays one page
/// ahead of the acknowledged one (read = acked + 1) with no transfer in flight. Plain retries every
/// minute changed nothing for ten hours (632 failures, the phone went from 20 % to 1 %); a relaunch
/// of the app (fresh notification subscriptions + client hello) cleared it at once.
///
/// Ladder, by failures in a row (repeats every 8):
///   1 retry in 20 s
///   2 re-acknowledge the strap's acknowledged page (HISTORICAL_DATA_RESULT, idempotent), retry
///   3 fresh session: WHOOP notifications off/on + client hello, retry
///   4 reconnect (the sync runs as soon as the link is ready again)
///   5-8 retries spaced 2 / 5 / 10 / 10 min
/// Periodic callers (5 min sync, sport, smart wake) wait meanwhile (`canStartPeriodicSync`), so a
/// stuck strap costs a handful of attempts per half hour instead of one a minute.
extension GooseBLEClient {
  func scheduleSyncRecovery(after message: String) {
    syncRecoveryWorkItem?.cancel()
    if syncFailureStreak == 0 {
      syncFailureStreakStartedAt = Date()
      syncRecoverySteps = []
    }
    syncFailureStreak += 1
    let phase = (syncFailureStreak - 1) % 8 + 1
    var delay: TimeInterval
    var step: String
    switch phase {
    case 1:
      step = "retry"
      delay = 20
    case 2:
      step = reacknowledgeStrapRange() ? "reack" : "retry"
      delay = 8
    case 3:
      step = "session"
      refreshStrapSession()
      delay = 12
    case 4:
      if reconnectForSyncRecovery() {
        syncRecoverySteps.append("reconnect")
        syncBackoffUntil = Date().addingTimeInterval(60)
        record(source: "ble.sync", title: "historical_sync.recovery", body: "step=reconnect streak=\(syncFailureStreak)")
        return
      }
      step = "retry"
      delay = 60
    default:
      step = "wait"
      delay = [120, 300, 600, 600][phase - 5]
    }
    syncRecoverySteps.append(step)
    syncBackoffUntil = Date().addingTimeInterval(delay - 1)
    record(source: "ble.sync", title: "historical_sync.recovery", body: "step=\(step) streak=\(syncFailureStreak) next_in=\(Int(delay))s")
    let work = DispatchWorkItem { [weak self] in
      guard let self, self.canSyncHistorical else { return }
      self.syncBackoffUntil = nil
      self.syncHistoricalPackets()
    }
    syncRecoveryWorkItem = work
    DispatchQueue.main.asyncAfter(deadline: .now() + delay, execute: work)
  }

  /// called when a sync completes
  func noteSyncRecovered(packets: Int) {
    syncRecoveryWorkItem?.cancel()
    syncBackoffUntil = nil
    guard syncFailureStreak > 0 else { return }
    let minutes = syncFailureStreakStartedAt.map { Int(Date().timeIntervalSince($0) / 60) } ?? 0
    record(
      source: "ble.sync",
      title: "historical_sync.recovered",
      body: "after \(syncFailureStreak) failures in \(minutes) min steps=\(syncRecoverySteps.joined(separator: ",")) packets=\(packets)"
    )
    syncFailureStreak = 0
    syncRecoverySteps = []
    syncFailureStreakStartedAt = nil
  }

  /// Back in the foreground: sync now if the data is more than 3 minutes old; a strap that was
  /// failing gets a fresh session first (what the morning relaunch did on 20 Nov).
  func syncOnForegroundIfStale() {
    guard canSyncHistorical else { return }
    let last = lastHistoricalSyncCompletedAt ?? .distantPast
    guard Date().timeIntervalSince(last) > 180 else { return }
    syncRecoveryWorkItem?.cancel()
    syncBackoffUntil = nil
    if syncFailureStreak > 0 {
      syncRecoverySteps.append("foreground_session")
      refreshStrapSession()
      DispatchQueue.main.asyncAfter(deadline: .now() + 3) { [weak self] in
        guard let self, self.canSyncHistorical else { return }
        self.syncHistoricalPackets()
      }
    } else {
      syncHistoricalPackets()
    }
  }

  /// development (gooseswift://sync-recovery-test?step=reack|session|reconnect): one step on a
  /// healthy strap, then a sync — each step must leave the next sync working
  func testSyncRecoveryStep(_ step: String) {
    record(source: "ble.sync", title: "historical_sync.recovery.test", body: "step=\(step)")
    switch step {
    case "reack": _ = reacknowledgeStrapRange(force: true)
    case "session": refreshStrapSession()
    case "reconnect": _ = reconnectForSyncRecovery(); return
    default: break
    }
    DispatchQueue.main.asyncAfter(deadline: .now() + 6) { [weak self] in
      guard let self, self.canSyncHistorical else { return }
      self.syncHistoricalPackets()
    }
  }

  /// Parsed from a valid GET_DATA_RANGE response (u32 words from body offset 1).
  static func historicalRangeWords(_ payload: [UInt8]) -> [UInt32] {
    let body = Array(payload.dropFirst(5))
    var words: [UInt32] = []
    var offset = 1
    while offset + 3 < body.count, offset < 25 {
      if let w = readUInt32LE(body, at: offset) { words.append(w) }
      offset += 4
    }
    return words
  }

  /// HISTORICAL_DATA_RESULT for the page the strap already holds as acknowledged (words[3], tag
  /// words[4]) — the same frame the app sends after every page, so nothing new is released; it
  /// gives a strap waiting on an acknowledgement its answer.
  func reacknowledgeStrapRange(force: Bool = false) -> Bool {
    let w = lastHistoricalRangeWords
    guard w.count >= 5, w[1] != w[3] || force, canSyncHistorical,
          let activePeripheral, let commandCharacteristic,
          let type = writeType(for: commandCharacteristic) else {
      return false
    }
    let payload: [UInt8] = [1] + Self.uint32LEBytes(w[3]) + Self.uint32LEBytes(w[4])
    let sequence = nextHistoricalSequence()
    let frame = Self.buildV5CommandFrame(sequence: sequence, command: HistoricalCommandKind.historicalDataResult.commandNumber, data: payload)
    activePeripheral.writeValue(frame, for: commandCharacteristic, type: type)
    record(source: "ble.sync", title: "historical_sync.recovery.reack", body: "seq=\(sequence) read=\(w[1]) acked=\(w[3]) payload=\(Data(payload).hexString)")
    return true
  }

  /// WHOOP notifications off then on, then a fresh client hello.
  func refreshStrapSession() {
    guard let peripheral = activePeripheral else { return }
    let chars = (peripheral.services ?? []).flatMap { $0.characteristics ?? [] }
      .filter { notificationCharacteristicIDs.contains($0.uuid) }
    for c in chars where c.isNotifying {
      peripheral.setNotifyValue(false, for: c)
    }
    record(source: "ble.sync", title: "historical_sync.recovery.session", body: "notifications=\(chars.count)")
    DispatchQueue.main.asyncAfter(deadline: .now() + 1) { [weak self] in
      guard let self, let p = self.activePeripheral, p === peripheral else { return }
      for c in chars {
        p.setNotifyValue(true, for: c)
      }
      self.sendClientHello(reason: "sync_recovery", force: true)
    }
  }

  func reconnectForSyncRecovery() -> Bool {
    guard let central, let peripheral = activePeripheral, peripheral.state == .connected,
          rememberedDeviceID == peripheral.identifier else {
      return false
    }
    pendingAutomaticHistoricalSyncReason = "sync_recovery"
    central.cancelPeripheralConnection(peripheral)
    return true
  }

  static func uint32LEBytes(_ v: UInt32) -> [UInt8] {
    [UInt8(v & 0xff), UInt8((v >> 8) & 0xff), UInt8((v >> 16) & 0xff), UInt8(v >> 24)]
  }
}

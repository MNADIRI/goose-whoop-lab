import Foundation
import SQLite3

/// whoop-lab: incremental export of raw strap frames to a user-chosen folder (typically iCloud Drive),
/// so the Mac analysis lab receives data without a cable.
///
/// Each export reads `raw_evidence` rows after the last exported rowid and writes one chunk:
///   <folder>/raw/<yyyyMMdd-HHmmss>-r<firstRowID>-<lastRowID>.tsv.deflate
/// The chunk is raw DEFLATE (Apple `.zlib`, no header; Python: `zlib.decompress(b, -15)`) of UTF-8 TSV lines:
///   rowid \t evidence_id \t captured_at \t payload_hex
/// The cursor only advances after the chunk is fully written, so a failed export is retried next time;
/// the Mac importer de-duplicates on evidence_id.
@MainActor
final class WhoopLabAutoExport: ObservableObject {
  static let autoHistoricalSyncKey = "whoopLab.autoHistoricalSync"
  static let syncInterval: TimeInterval = 30 * 60
  static let exportInterval: TimeInterval = 10 * 60
  nonisolated static let maxRowsPerChunk = 200_000

  @Published private(set) var folderName: String?
  @Published private(set) var status = "No folder selected"
  @Published private(set) var lastExportAt: Date?
  @Published var autoHistoricalSync: Bool {
    didSet { defaults.set(autoHistoricalSync, forKey: Self.autoHistoricalSyncKey) }
  }

  private let defaults = UserDefaults.standard
  private let bookmarkKey = "whoopLab.export.folderBookmark"
  private let cursorKey = "whoopLab.export.lastRowID"
  private let lastExportKey = "whoopLab.export.lastExportAt"
  private let workQueue = DispatchQueue(label: "whoop-lab.export", qos: .utility)
  private var exportInFlight = false
  private var timer: Timer?
  private weak var ble: GooseBLEClient?

  init() {
    defaults.register(defaults: [Self.autoHistoricalSyncKey: true])
    autoHistoricalSync = defaults.bool(forKey: Self.autoHistoricalSyncKey)
    lastExportAt = defaults.object(forKey: lastExportKey) as? Date
    if let url = resolveFolder() {
      folderName = url.lastPathComponent
      status = lastExportAt.map { "Last export \(Self.displayFormatter.string(from: $0))" } ?? "Ready"
    }
  }

  /// Starts the periodic check: history sync every 30 min while connected, export every 10 min.
  /// BLE notifications keep the app running in the background (bluetooth-central), so the timer keeps firing.
  func start(ble: GooseBLEClient) {
    self.ble = ble
    timer?.invalidate()
    let timer = Timer(timeInterval: 60, repeats: true) { [weak self] _ in
      Task { @MainActor in self?.tick() }
    }
    timer.tolerance = 15
    RunLoop.main.add(timer, forMode: .common)
    self.timer = timer
  }

  func tick(now: Date = Date()) {
    if autoHistoricalSync, let ble, ble.canSyncHistorical {
      let last = ble.lastHistoricalSyncCompletedAt ?? .distantPast
      if now.timeIntervalSince(last) >= Self.syncInterval {
        ble.record(source: "whoop-lab", title: "auto_sync.requested", body: "since_last=\(Int(now.timeIntervalSince(last)))s")
        ble.syncHistoricalPackets()
        return
      }
    }
    let lastExport = lastExportAt ?? .distantPast
    if now.timeIntervalSince(lastExport) >= Self.exportInterval {
      exportNow(reason: "periodic")
    }
  }

  func historicalSyncFinished(_ progress: GooseHistoricalSyncProgress) {
    guard progress.isTerminal, !progress.failed else {
      return
    }
    exportNow(reason: "history_sync")
  }

  func setFolder(_ url: URL) {
    let accessing = url.startAccessingSecurityScopedResource()
    defer { if accessing { url.stopAccessingSecurityScopedResource() } }
    do {
      let bookmark = try url.bookmarkData(options: [], includingResourceValuesForKeys: nil, relativeTo: nil)
      defaults.set(bookmark, forKey: bookmarkKey)
      folderName = url.lastPathComponent
      status = "Folder set"
      exportNow(reason: "folder_selected")
    } catch {
      status = "Folder error: \(error.localizedDescription)"
    }
  }

  func exportNow(reason: String) {
    guard !exportInFlight, let folder = resolveFolder() else {
      return
    }
    exportInFlight = true
    status = "Exporting…"
    let databasePath = HealthDataStore.defaultDatabasePath()
    let cursor = Int64(defaults.integer(forKey: cursorKey))
    workQueue.async { [weak self] in
      let result = Result { try Self.writeChunks(databasePath: databasePath, folder: folder, after: cursor) }
      Task { @MainActor in
        self?.finishExport(result, reason: reason)
      }
    }
  }

  private func finishExport(_ result: Result<ExportOutcome, Error>, reason: String) {
    exportInFlight = false
    let now = Date()
    switch result {
    case .success(let outcome):
      defaults.set(Int(outcome.lastRowID), forKey: cursorKey)
      defaults.set(now, forKey: lastExportKey)
      lastExportAt = now
      status = outcome.rows == 0
        ? "Up to date (\(Self.displayFormatter.string(from: now)))"
        : "Exported \(outcome.rows) frames in \(outcome.files) file\(outcome.files == 1 ? "" : "s") (\(Self.displayFormatter.string(from: now)))"
      ble?.record(source: "whoop-lab", title: "export.completed", body: "reason=\(reason) rows=\(outcome.rows) files=\(outcome.files) cursor=\(outcome.lastRowID)")
    case .failure(let error):
      status = "Export failed: \(error.localizedDescription)"
      ble?.record(level: .warn, source: "whoop-lab", title: "export.failed", body: "reason=\(reason) \(error.localizedDescription)")
    }
  }

  private func resolveFolder() -> URL? {
    guard let bookmark = defaults.data(forKey: bookmarkKey) else {
      return nil
    }
    var stale = false
    guard let url = try? URL(resolvingBookmarkData: bookmark, options: [], relativeTo: nil, bookmarkDataIsStale: &stale) else {
      status = "Folder unavailable, pick it again"
      return nil
    }
    if stale, let refreshed = try? url.bookmarkData(options: [], includingResourceValuesForKeys: nil, relativeTo: nil) {
      defaults.set(refreshed, forKey: bookmarkKey)
    }
    return url
  }

  // MARK: - Background work

  struct ExportOutcome {
    let rows: Int
    let files: Int
    let lastRowID: Int64
  }

  enum ExportError: LocalizedError {
    case sqlite(String)
    case compression
    case folderAccess

    var errorDescription: String? {
      switch self {
      case .sqlite(let message): "SQLite: \(message)"
      case .compression: "compression failed"
      case .folderAccess: "no access to the export folder"
      }
    }
  }

  nonisolated static func writeChunks(databasePath: String, folder: URL, after cursor: Int64) throws -> ExportOutcome {
    var db: OpaquePointer?
    guard sqlite3_open_v2(databasePath, &db, SQLITE_OPEN_READONLY, nil) == SQLITE_OK, let db else {
      throw ExportError.sqlite("cannot open database")
    }
    defer { sqlite3_close(db) }
    sqlite3_busy_timeout(db, 5_000)

    // A reset database (fresh install) restarts rowids; export everything again in that case.
    var start = cursor
    if let maxRowID = try scalarInt64(db, "SELECT IFNULL(MAX(rowid), 0) FROM raw_evidence"), maxRowID < cursor {
      start = 0
    }

    guard folder.startAccessingSecurityScopedResource() else {
      throw ExportError.folderAccess
    }
    defer { folder.stopAccessingSecurityScopedResource() }
    let rawDirectory = folder.appendingPathComponent("raw", isDirectory: true)
    try FileManager.default.createDirectory(at: rawDirectory, withIntermediateDirectories: true)

    var totalRows = 0
    var files = 0
    var last = start
    while true {
      let chunk = try readChunk(db, after: last, limit: maxRowsPerChunk)
      guard let first = chunk.firstRowID else {
        break
      }
      guard let compressed = try? (chunk.text.data(using: .utf8)! as NSData).compressed(using: .zlib) as Data else {
        throw ExportError.compression
      }
      let name = "\(fileStamp.string(from: Date()))-r\(first)-\(chunk.lastRowID).tsv.deflate"
      try coordinatedWrite(compressed, to: rawDirectory.appendingPathComponent(name))
      totalRows += chunk.rows
      files += 1
      last = chunk.lastRowID
      if chunk.rows < maxRowsPerChunk {
        break
      }
    }
    return ExportOutcome(rows: totalRows, files: files, lastRowID: last)
  }

  nonisolated private static func readChunk(_ db: OpaquePointer, after rowID: Int64, limit: Int) throws
    -> (text: String, rows: Int, firstRowID: Int64?, lastRowID: Int64) {
    let sql = "SELECT rowid, evidence_id, captured_at, payload_hex FROM raw_evidence WHERE rowid > ? ORDER BY rowid LIMIT ?"
    var statement: OpaquePointer?
    guard sqlite3_prepare_v2(db, sql, -1, &statement, nil) == SQLITE_OK, let statement else {
      throw ExportError.sqlite(String(cString: sqlite3_errmsg(db)))
    }
    defer { sqlite3_finalize(statement) }
    sqlite3_bind_int64(statement, 1, rowID)
    sqlite3_bind_int(statement, 2, Int32(limit))

    var text = ""
    var rows = 0
    var first: Int64?
    var last = rowID
    while true {
      let step = sqlite3_step(statement)
      if step == SQLITE_DONE {
        break
      }
      guard step == SQLITE_ROW else {
        throw ExportError.sqlite(String(cString: sqlite3_errmsg(db)))
      }
      let id = sqlite3_column_int64(statement, 0)
      first = first ?? id
      last = id
      text += "\(id)\t\(column(statement, 1))\t\(column(statement, 2))\t\(column(statement, 3))\n"
      rows += 1
    }
    return (text, rows, first, last)
  }

  nonisolated private static func column(_ statement: OpaquePointer, _ index: Int32) -> String {
    guard let value = sqlite3_column_text(statement, index) else {
      return ""
    }
    return String(cString: value)
  }

  nonisolated private static func scalarInt64(_ db: OpaquePointer, _ sql: String) throws -> Int64? {
    var statement: OpaquePointer?
    guard sqlite3_prepare_v2(db, sql, -1, &statement, nil) == SQLITE_OK, let statement else {
      throw ExportError.sqlite(String(cString: sqlite3_errmsg(db)))
    }
    defer { sqlite3_finalize(statement) }
    return sqlite3_step(statement) == SQLITE_ROW ? sqlite3_column_int64(statement, 0) : nil
  }

  /// Writes through NSFileCoordinator so iCloud Drive picks the file up reliably.
  nonisolated private static func coordinatedWrite(_ data: Data, to url: URL) throws {
    var coordinationError: NSError?
    var writeError: Error?
    NSFileCoordinator().coordinate(writingItemAt: url, options: .forReplacing, error: &coordinationError) { target in
      do {
        try data.write(to: target, options: .atomic)
      } catch {
        writeError = error
      }
    }
    if let error = coordinationError ?? writeError {
      throw error
    }
  }

  nonisolated private static let fileStamp: DateFormatter = {
    let formatter = DateFormatter()
    formatter.locale = Locale(identifier: "en_US_POSIX")
    formatter.timeZone = TimeZone(identifier: "UTC")
    formatter.dateFormat = "yyyyMMdd-HHmmss"
    return formatter
  }()

  static let displayFormatter: DateFormatter = {
    let formatter = DateFormatter()
    formatter.dateStyle = .none
    formatter.timeStyle = .short
    return formatter
  }()
}

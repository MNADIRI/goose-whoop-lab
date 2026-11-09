import Foundation
import CryptoKit
import SwiftUI
import UIKit

#if canImport(HealthKit)
import HealthKit
#endif

extension MoreDataStore {
  nonisolated static func writeRawValidationSidecars(
    _ manifest: [String: Any],
    review: [String: Any],
    reviewStatus: String,
    runbookMarkdown: String,
    bundlePath: String,
    outputDirectory: String
  ) throws -> RawValidationSidecarResult {
    let fileManager = FileManager.default
    var isDirectory: ObjCBool = false
    let bundleURL = URL(fileURLWithPath: bundlePath)
    let sidecarDirectory: URL
    if fileManager.fileExists(atPath: bundlePath, isDirectory: &isDirectory), isDirectory.boolValue {
      sidecarDirectory = bundleURL
    } else {
      sidecarDirectory = URL(fileURLWithPath: outputDirectory, isDirectory: true)
    }
    try fileManager.createDirectory(at: sidecarDirectory, withIntermediateDirectories: true)
    let sidecarURL = sidecarDirectory.appendingPathComponent("local-health-validation-manifest.json")
    let data = try JSONSerialization.data(withJSONObject: manifest, options: [.prettyPrinted, .sortedKeys])
    try data.write(to: sidecarURL, options: .atomic)
    let reviewURL = sidecarDirectory.appendingPathComponent("local-health-validation-review.json")
    let reviewData = try JSONSerialization.data(withJSONObject: review, options: [.prettyPrinted, .sortedKeys])
    try reviewData.write(to: reviewURL, options: .atomic)
    let runbookURL = sidecarDirectory.appendingPathComponent("local-health-validation-runbook.md")
    try runbookMarkdown.write(to: runbookURL, atomically: true, encoding: .utf8)
    return RawValidationSidecarResult(
      manifestStatus: "Saved \(sidecarURL.lastPathComponent)",
      manifestURL: sidecarURL,
      reviewStatus: reviewStatus,
      reviewURL: reviewURL,
      runbookStatus: "Saved \(runbookURL.lastPathComponent)",
      runbookURL: runbookURL
    )
  }

  nonisolated static func rawValidationReviewSummary(_ review: [String: Any]) -> String {
    let status = firstString(review, keys: ["status"]) ?? "Manifest reviewed"
    let placeholders = firstString(review, keys: ["placeholder_field_count"]) ?? "0"
    let sessionBindings = firstString(review, keys: ["capture_session_binding_required_case_count"]) ?? "0"
    let labelPolicy = firstString(review, keys: ["label_policy_valid"]) ?? "false"
    return "\(status): \(placeholders) fields, \(sessionBindings) session bindings, labels_are_labels=\(labelPolicy)"
  }

  nonisolated static func rawValidationRunbookMarkdown(
    bridge: GooseRustBridge,
    manifest: [String: Any]
  ) throws -> String {
    let result = try bridge.request(
      method: "validation.local_health_manifest_runbook",
      args: ["manifest": manifest]
    )
    if let markdown = firstString(result, keys: ["markdown"]), !markdown.isEmpty {
      return markdown
    }
    throw NSError(
      domain: "GooseRawValidationRunbook",
      code: 1,
      userInfo: [NSLocalizedDescriptionKey: "Rust runbook renderer returned no markdown"]
    )
  }

  func runFullRawExport() {
    rawExportStart = Self.fullExportStart
    rawExportEnd = Date().addingTimeInterval(60).moreISO8601String()
    rawCaptureSessions = ""
    rawPacketTypes = ""
    rawSensorSignals = ""
    rawMetricFamilies = ""
    rawAlgorithmIDs = ""
    rawAlgorithmVersions = ""
    includeRawBytes = true
    selectedRawFamilies = Set(Self.rawFamilies)
    runRawExport()
  }

  func saveLocalDataBundle() {
    guard !localExportInProgress else {
      localExportStatus = "Local export already running"
      return
    }
    localExportInProgress = true
    localExportStatus = "Saving local data file..."
    localExportURL = nil
    localExportManifestURL = nil

    DispatchQueue.global(qos: .userInitiated).async {
      do {
        let result = try GooseLocalDataExporter.createBundle()
        DispatchQueue.main.async {
          self.localExportInProgress = false
          self.localExportStatus = "Saved \(result.fileCount) files, \(Self.byteCountText(result.byteCount))\(result.manifestStatusSuffix) | \(result.validation.summary)"
          self.localExportURL = result.url
          self.localExportManifestURL = result.manifestURL
        }
      } catch {
        DispatchQueue.main.async {
          self.localExportInProgress = false
          self.localExportStatus = "Local export failed: \(Self.errorSummary(error))"
        }
      }
    }
  }

  func validateExportArtifacts() {
    let artifactValidation = Self.validateRawExportArtifacts(
      bridge: bridge,
      bundlePath: rawBundlePath,
      zipPath: rawZipPath
    )
    rawBundleValidation = artifactValidation.bundleValidation
    rawZipValidation = artifactValidation.zipValidation
    privacyLintStatus = artifactValidation.privacyLint
    sanitizedPrivacyStatus = artifactValidation.sanitizedPrivacy
  }

  func validationStatusKind(_ summary: String) -> MoreStatusKind {
    Self.statusKind(forValidationSummary: summary)
  }

  func applyRecommendedAlgorithmDefaults(healthStore: HealthDataStore) {
    healthStore.loadBridgeCatalogsIfNeeded()
    for family in healthStore.algorithmFamilies {
      guard let first = healthStore.algorithms(for: family).first else {
        continue
      }
      healthStore.selectAlgorithm(first.id, for: family)
    }

    guard databaseExists else {
      algorithmPreferenceStatus = "Applied in-memory defaults; database persistence unavailable"
      return
    }

    do {
      let value = try bridge.request(
        method: "settings.apply_default_algorithm_preferences",
        args: [
          "database_path": databasePath,
          "scope": "primary",
        ]
      )
      algorithmPreferenceStatus = "Defaults persisted: \(Self.shortBridgeSummary(value))"
    } catch {
      algorithmPreferenceStatus = "Defaults failed: \(Self.errorSummary(error))"
    }
  }

  func persistAlgorithmPreference(family: String, algorithm: HealthAlgorithmDefinition) {
    guard databaseExists else {
      algorithmPreferenceStatus = "Selected \(algorithm.displayName); persistence waits for database"
      return
    }

    do {
      let value = try bridge.request(
        method: "settings.set_algorithm_preference",
        args: [
          "database_path": databasePath,
          "scope": "primary",
          "metric_family": family,
          "algorithm_id": algorithm.id,
          "version": "0.1.0",
          "register_built_ins": true,
        ]
      )
      algorithmPreferenceStatus = "Preference saved: \(Self.shortBridgeSummary(value))"
    } catch {
      algorithmPreferenceStatus = "Preference save failed: \(Self.errorSummary(error))"
    }
  }

  func runFrameParseProbe() {
    do {
      let value = try bridge.request(
        method: "protocol.parse_frame_hex",
        args: [
          "device_type": "GOOSE",
          "frame_hex": GooseHello.clientHelloFrameHex,
        ]
      )
      frameParseStatus = "Parsed \(Self.firstString(value, keys: ["packet_type_name", "packet_type"]) ?? "frame")"
      frameCRCStatus = Self.firstString(value, keys: ["crc_status", "crc", "checksum"]) ?? "CRC accepted by parser"
      framePayloadStatus = Self.shortBridgeSummary(value["parsed_payload"] as? [String: Any] ?? value)
      let warnings = (value["warnings"] as? [Any])?.count ?? 0
      frameWarningsStatus = warnings == 0 ? "No warnings" : "\(warnings) warnings"
      frameTimelineStatus = "Timeline generation waits for decoded frame rows"
    } catch {
      frameParseStatus = "Parse failed: \(Self.errorSummary(error))"
    }
  }

  func startDebugSession() {
    do {
      let value = try bridge.request(
        method: "debug.start_session",
        args: [
          "database_path": databasePath,
          "session_id": debugSessionID,
          "started_at_unix_ms": Self.unixMilliseconds(Date()),
          "bridge": [
            "url": "ws://127.0.0.1:8765",
            "bind_host": "127.0.0.1",
            "token_required": true,
            "token_present": false,
            "remote_bind_enabled": false,
            "visible_remote_bind_toggle": true,
          ],
        ]
      )
      debugWebSocketStatus = "Session \(debugSessionID.prefix(12)) started"
      debugNextAction = Self.shortBridgeSummary(value)
    } catch {
      debugWebSocketStatus = "Debug session failed: \(Self.errorSummary(error))"
      debugNextAction = "Check database path and Rust bridge"
    }
  }

  func refreshDebugSnapshot() {
    do {
      let value = try bridge.request(
        method: "debug.session_snapshot",
        args: [
          "database_path": databasePath,
          "session_id": debugSessionID,
        ]
      )
      debugNextAction = Self.shortBridgeSummary(value)
    } catch {
      debugNextAction = "Snapshot failed: \(Self.errorSummary(error))"
    }
  }

  func runUICoverageAudit() {
    do {
      let value = try bridge.request(method: "ui_coverage.audit")
      uiCoverageStatus = Self.passSummary(value, fallback: Self.shortBridgeSummary(value))
      deferredSurfaceStatus = Self.firstString(value, keys: ["deferred_surfaces", "missing_surfaces", "blocked_surfaces"]) ?? "See audit output"
    } catch {
      uiCoverageStatus = "Coverage audit failed: \(Self.errorSummary(error))"
      deferredSurfaceStatus = "Audit unavailable"
    }
  }

  func runPropertySuite() {
    do {
      let value = try bridge.request(
        method: "diagnostics.property_suite",
        args: [
          "seed": 42,
          "cases_per_group": 16,
        ]
      )
      propertySuiteStatus = Self.passSummary(value, fallback: Self.shortBridgeSummary(value))
    } catch {
      propertySuiteStatus = "Property suite failed: \(Self.errorSummary(error))"
    }
  }

  func runPerfBudget() {
    do {
      let value = try bridge.request(
        method: "diagnostics.perf_budget",
        args: [
          "scale": 1,
        ]
      )
      perfBudgetStatus = Self.passSummary(value, fallback: Self.shortBridgeSummary(value))
    } catch {
      perfBudgetStatus = "Perf budget failed: \(Self.errorSummary(error))"
    }
  }

  func runCaptureArrivalPlan() {
    guard databaseExists else {
      commandCapturePlanStatus = "Blocked: local database unavailable"
      return
    }
    do {
      let value = try bridge.request(
        method: "capture.arrival_plan",
        args: [
          "database_path": databasePath,
          "start": rawExportStart,
          "end": rawExportEnd,
          "timezone": TimeZone.current.identifier,
          "min_owned_captures": 1,
          "require_owned_captures": true,
          "require_scores_ready": true,
        ]
      )
      commandCapturePlanStatus = Self.captureArrivalPlanSummary(value)
    } catch {
      commandCapturePlanStatus = "Capture plan failed: \(Self.errorSummary(error))"
    }
  }

  func loadCommandDefinitions() {
    do {
      let value = try bridge.requestValue(method: "commands.definitions")
      commandGroups = MoreCommandGroup.groups(from: value)
      commandGateSweepStatus = "Definitions loaded"
    } catch {
      commandGateSweepStatus = "Command definitions failed: \(Self.errorSummary(error))"
      commandGroups = MoreCommandGroup.defaults
    }
  }

  func showDestructiveGate() {
    destructiveGateStatus = "Locked behind explicit confirmation; no direct command sent"
  }

  static func previewDefault() -> MoreDataStore {
    let store = MoreDataStore(databasePath: "/tmp/goose-preview.sqlite")
    store.coreVersionStatus = "Rust core 0.1.0"
    store.schemaVersion = "preview"
    store.storageStatus = "Preview no database"
    return store
  }

  static func previewConnected() -> MoreDataStore {
    let store = previewDefault()
    store.captureStatus = "Ready for connected strap"
    store.liveCaptureStatus = "Ready; notifications are mirrored through GooseBLEClient.onNotification"
    store.recentCaptureSessions = []
    store.healthSyncReports = ["Apple Health metric sync disabled; profile weight autofill only"]
    return store
  }

  static func previewDebugHeavy() -> MoreDataStore {
    let store = previewConnected()
    store.frameParseStatus = "Parsed GET_HELLO"
    store.frameCRCStatus = "CRC accepted"
    store.framePayloadStatus = "identity request payload"
    store.frameWarningsStatus = "No warnings"
    store.debugWebSocketStatus = "Session preview started"
    store.uiCoverageStatus = "2 deferred surfaces"
    store.propertySuiteStatus = "128 cases passed"
    store.perfBudgetStatus = "Within budget"
    store.destructiveGateStatus = "Locked"
    return store
  }

  static let fullExportStart = "1970-01-01T00:00:00Z"
  static let healthFamilies: [String] = []
  static let unavailableHealthFamilies = ["heart_rate", "resting_heart_rate", "hrv", "respiratory_rate", "steps", "activity", "oxygen_saturation", "skin_temperature", "sleep", "active_energy"]
  static let rawFamilies = [
    "raw_evidence",
    "decoded_frames",
    "packet_timeline",
    "sensor_samples",
    "metric_features",
    "metric_outputs",
    "algorithm_runs",
    "calibration_labels",
    "calibration_runs",
    "activity_sessions",
    "activity_metrics",
    "activity_intervals",
    "activity_labels",
    "local_health_metrics",
    "debug_sessions",
    "debug_commands",
    "debug_events",
    "command_validation",
    "sqlite",
  ]

  func healthPermissionGrants() -> [String] {
    ["bodyMass"]
  }

  func healthSyncCandidate(family: String, index: Int) -> [String: Any] {
    let semantic: String
    let unit: String
    let value: Double
    switch family {
    case "heart_rate":
      semantic = "heart_rate"
      unit = "count/min"
      value = 62
    case "resting_heart_rate":
      semantic = "resting_heart_rate"
      unit = "count/min"
      value = 55
    case "hrv":
      semantic = "hrv_rmssd"
      unit = "ms"
      value = 38
    case "respiratory_rate":
      semantic = "respiratory_rate"
      unit = "count/min"
      value = 16
    case "steps":
      semantic = "steps"
      unit = "count"
      value = 1200
    case "activity":
      semantic = "activity"
      unit = "min"
      value = 30
    default:
      semantic = family
      unit = "count"
      value = 1
    }

    return [
      "record_id": "swift-more-\(family)-\(index)",
      "metric_family": family,
      "semantic": semantic,
      "source_kind": "swift_more_dry_run",
      "start_time": healthBackfillStart,
      "end_time": healthBackfillEnd,
      "value": value,
      "unit": unit,
      "approved_by_user": true,
      "provenance": [
        "surface": "MoreHealthSyncView",
        "dry_run": true,
      ],
    ]
  }

  static func applicationDirectory() -> URL {
    let baseDirectory = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask).first
      ?? FileManager.default.temporaryDirectory
    let directory = baseDirectory.appendingPathComponent("GooseSwift", isDirectory: true)
    try? FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
    return directory
  }

  static func documentsApplicationDirectory() -> URL {
    let baseDirectory = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask).first
      ?? FileManager.default.temporaryDirectory
    let directory = baseDirectory.appendingPathComponent("GooseSwift", isDirectory: true)
    try? FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
    return directory
  }

  // whoop-lab: a formatter was created per call from SwiftUI bodies (2 s of main thread per minute).
  nonisolated(unsafe) private static let iso8601Formatter = ISO8601DateFormatter()

  static func parseISO8601(_ text: String) -> Date? {
    iso8601Formatter.date(from: text)
  }

  static func unixMilliseconds(_ date: Date) -> Int64 {
    Int64((date.timeIntervalSince1970 * 1000).rounded())
  }

  static func csvValues(_ text: String) -> [String] {
    text.split(separator: ",")
      .map { $0.trimmingCharacters(in: .whitespacesAndNewlines) }
      .filter { !$0.isEmpty }
  }

  nonisolated static func firstString(_ dictionary: [String: Any], keys: [String]) -> String? {
    for key in keys {
      if let value = dictionary[key] {
        return stringValue(value)
      }
    }
    return nil
  }

  nonisolated static func stringValue(_ value: Any) -> String {
    if let string = value as? String {
      return string
    }
    if let bool = value as? Bool {
      return bool ? "true" : "false"
    }
    if let number = value as? NSNumber {
      return number.stringValue
    }
    if let array = value as? [Any] {
      return "\(array.count)"
    }
    if let dictionary = value as? [String: Any] {
      return dictionary.keys.sorted().prefix(3).joined(separator: ", ")
    }
    return String(describing: value)
  }

  nonisolated static func passSummary(_ dictionary: [String: Any], fallback: String) -> String {
    if let pass = dictionary["pass"] as? Bool {
      return pass ? "Passed" : "Blocked"
    }
    if let ok = dictionary["ok"] as? Bool {
      return ok ? "OK" : "Failed"
    }
    if let status = firstString(dictionary, keys: ["status", "result", "state"]) {
      return status
    }
    return fallback
  }

  nonisolated static func validateRawExportArtifacts(
    bridge: GooseRustBridge,
    bundlePath: String,
    zipPath: String
  ) -> RawExportArtifactValidationResult {
    guard bundlePath != "No bundle" else {
      return RawExportArtifactValidationResult(
        bundleValidation: "No bundle to validate",
        zipValidation: zipPath == "No zip" ? "No zip to validate" : "Not validated",
        privacyLint: "No bundle to lint",
        sanitizedPrivacy: "No sanitized copy"
      )
    }

    let bundleValidation: String
    do {
      let bundle = try bridge.request(method: "export.validate_bundle", args: ["path": bundlePath])
      bundleValidation = passSummary(bundle, fallback: shortBridgeSummary(bundle))
    } catch {
      bundleValidation = "Bundle validation failed: \(errorSummary(error))"
    }

    let zipValidation: String
    if zipPath == "No zip" {
      zipValidation = "No zip to validate"
    } else {
      do {
        let zip = try bridge.request(method: "export.validate_bundle", args: ["path": zipPath])
        zipValidation = passSummary(zip, fallback: shortBridgeSummary(zip))
      } catch {
        zipValidation = "Zip validation failed: \(errorSummary(error))"
      }
    }

    let privacyLint: String
    let sanitizedPrivacy: String
    do {
      let lint = try bridge.request(method: "privacy.lint", args: ["path": bundlePath])
      privacyLint = passSummary(lint, fallback: shortBridgeSummary(lint))
      sanitizedPrivacy = "Linted; sanitized copy action pending"
    } catch {
      privacyLint = "Privacy lint failed: \(errorSummary(error))"
      sanitizedPrivacy = "No sanitized copy"
    }

    return RawExportArtifactValidationResult(
      bundleValidation: bundleValidation,
      zipValidation: zipValidation,
      privacyLint: privacyLint,
      sanitizedPrivacy: sanitizedPrivacy
    )
  }

  static func statusKind(forValidationSummary summary: String) -> MoreStatusKind {
    let normalized = summary.trimmingCharacters(in: .whitespacesAndNewlines).lowercased()
    if normalized.hasPrefix("no ") {
      return .unavailable
    }
    if normalized.contains("failed")
      || normalized.contains("blocked")
      || normalized.contains("operator_edits_required")
    {
      return .blocked
    }
    if normalized == "passed"
      || normalized == "ok"
      || normalized.hasPrefix("linted")
      || normalized.contains("ready_to_run_validation_suite")
    {
      return .ready
    }
    return .pending
  }

  static func nextActionSummary(_ dictionary: [String: Any], fallback: String) -> String {
    if let nextActions = dictionary["next_actions"] as? [[String: Any]], let first = nextActions.first {
      return firstString(first, keys: ["action", "reason"]) ?? fallback
    }
    if let nextActions = dictionary["next_actions"] as? [Any], !nextActions.isEmpty {
      return "\(nextActions.count) next actions"
    }
    return fallback
  }

  nonisolated static func captureArrivalPlanSummary(_ dictionary: [String: Any]) -> String {
    var rows = [passSummary(dictionary, fallback: shortBridgeSummary(dictionary))]
    if let actionCount = firstString(dictionary, keys: ["action_count"]) {
      rows.append("\(actionCount) actions")
    }
    if let actions = dictionary["actions"] as? [[String: Any]] {
      let localHealthCount = actions.filter {
        firstString($0, keys: ["source"]) == "Local Health Validation"
      }.count
      if localHealthCount > 0 {
        rows.append("local-health validation \(localHealthCount)")
      }
    }
    if let review = dictionary["local_health_validation_review"] as? [String: Any],
       let openCases = firstString(review, keys: ["acceptance_evidence_open_case_count"]) {
      rows.append("open evidence \(openCases)")
    }
    if let focus = dictionary["next_capture_focus"] as? [String: Any],
       let source = firstString(focus, keys: ["source"]),
       let scope = firstString(focus, keys: ["scope"]) {
      rows.append("next \(source): \(scope)")
    }
    return rows.joined(separator: " | ")
  }

  static func recordCountSummary(_ dictionary: [String: Any]) -> String {
    if let counts = dictionary["table_counts"] as? [String: Any] {
      let rendered = counts.keys.sorted().prefix(4).map { "\($0): \(stringValue(counts[$0] ?? ""))" }
      return rendered.isEmpty ? "No table counts" : rendered.joined(separator: ", ")
    }
    return firstString(dictionary, keys: ["records", "record_count", "row_count"]) ?? "Storage checked"
  }

  nonisolated static func rowCountSummary(_ dictionary: [String: Any]) -> String {
    if let rowCounts = dictionary["row_counts"] as? [String: Any] {
      let rendered = rowCounts.keys.sorted().prefix(5).map { "\($0): \(stringValue(rowCounts[$0] ?? ""))" }
      return rendered.isEmpty ? "No rows exported" : rendered.joined(separator: ", ")
    }
    return firstString(dictionary, keys: ["rows", "row_count", "record_count"]) ?? "Rows reported in bundle manifest"
  }

  static func byteCountText(_ byteCount: UInt64) -> String {
    let formatter = ByteCountFormatter()
    formatter.allowedUnits = [.useKB, .useMB, .useGB]
    formatter.countStyle = .file
    return formatter.string(fromByteCount: Int64(byteCount))
  }

  nonisolated static func shortBridgeSummary(_ dictionary: [String: Any]) -> String {
    if dictionary.isEmpty {
      return "OK"
    }
    let priorityKeys = ["schema", "status", "pass", "session_id", "bundle_path", "command_count"]
    let parts = priorityKeys.compactMap { key -> String? in
      guard let value = dictionary[key] else {
        return nil
      }
      return "\(key)=\(stringValue(value))"
    }
    if !parts.isEmpty {
      return parts.joined(separator: ", ")
    }
    return dictionary.keys.sorted().prefix(3).joined(separator: ", ")
  }

  static func healthReportSummaries(from dictionary: [String: Any]) -> [String] {
    var rows: [String] = []
    rows.append(passSummary(dictionary, fallback: shortBridgeSummary(dictionary)))
    if let planned = dictionary["planned_writes"] as? [Any] {
      rows.append("\(planned.count) planned writes")
    }
    if let blocked = dictionary["blocked_writes"] as? [Any] {
      rows.append("\(blocked.count) blocked writes")
    }
    if let deletes = dictionary["planned_deletes"] as? [Any] {
      rows.append("\(deletes.count) planned deletes")
    }
    if let next = dictionary["next_actions"] as? [Any], !next.isEmpty {
      rows.append("\(next.count) next actions")
    }
    return rows
  }

  static func captureSessionSummaries(from dictionary: [String: Any]) -> [String] {
    let arrays = ["sessions", "capture_sessions", "items"].compactMap { dictionary[$0] as? [[String: Any]] }
    guard let sessions = arrays.first, !sessions.isEmpty else {
      return ["No stored capture sessions"]
    }
    return sessions.prefix(6).map { session in
      let id = firstString(session, keys: ["session_id", "id"]) ?? "session"
      let frames = firstString(session, keys: ["frame_count", "frames"]) ?? "0"
      return "\(id.prefix(12)) | \(frames) frames"
    }
  }

  nonisolated static func errorSummary(_ error: Error) -> String {
    if case let GooseRustBridgeError.methodFailed(message) = error {
      return message
    }
    return String(describing: error)
  }

  static var appVersion: String {
    let short = Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String ?? "0"
    let build = Bundle.main.object(forInfoDictionaryKey: "CFBundleVersion") as? String ?? "0"
    return "\(short) (\(build))"
  }
}

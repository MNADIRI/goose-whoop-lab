import Foundation

// Shared by the app and the widget extension (both targets compile this file).
// The snapshot is produced by the Rust engine (`daily.snapshot`, Rust/core/src/daily_context.rs):
// context, cards, metrics with their quality and a precomputed timeline. The widget only reads it.

enum DailyContext: String, Codable {
  case morning, daytime, evening, night
}

struct HealthMetric: Codable, Identifiable, Hashable {
  let id: String
  let label: String
  let value: Double?
  let unit: String
  let display: String
  let detail: String?
  let observedAt: Int?
  let computedAt: Int?
  let validUntil: Int?
  let liveUntil: Int?
  let live: Bool
  let source: String
  let algorithm: String
  /// measured / derived / estimated / calibrating / incomplete / stale / missing
  let quality: String
  let missingReason: String?
  /// good / bad / neutral
  let tone: String

  var isMissing: Bool { quality == "missing" }
  var isStale: Bool { quality == "stale" }
}

struct DailyInsight: Codable, Hashable {
  let id: String
  let text: String
  let metricIds: [String]
}

struct DailyHealthCard: Codable, Hashable {
  let context: DailyContext
  let title: String
  let hero: String
  let primary: [HealthMetric]
  let secondary: [HealthMetric]
  let insight: DailyInsight
  let pending: Bool
  let notRefreshedSince: Int?

  func metric(_ id: String) -> HealthMetric? { (primary + secondary).first { $0.id == id } }
  var heroMetric: HealthMetric? { metric(hero) }
}

struct DailyHealthEntryData: Codable, Hashable {
  let at: Int
  let context: DailyContext
  let card: DailyHealthCard
}

struct DailyTransition: Codable, Hashable {
  let at: Int
  let to: DailyContext
  let expected: Bool
  let reason: String
}

struct DailyHealthSnapshot: Codable {
  let schema: String
  let engineVersion: String
  let generatedAt: Int
  let utcOffsetSeconds: Int
  let context: DailyContext
  let contextSince: Int
  let trigger: String
  let transitions: [DailyTransition]
  let entries: [DailyHealthEntryData]

  var generatedDate: Date { Date(timeIntervalSince1970: TimeInterval(generatedAt)) }

  /// The precomputed entry valid at `date` (the last one at or before it).
  func entry(at date: Date) -> DailyHealthEntryData? {
    let t = Int(date.timeIntervalSince1970)
    return entries.last { $0.at <= t } ?? entries.first
  }
}

/// App Group store: the app writes the snapshot, the widget extension reads it.
enum DailyHealthSnapshotStore {
  static let appGroup = "group.com.marwannadiri.goose"
  static let fileName = "daily_health_snapshot.json"
  static let widgetKind = "MNHealthDailyWidget"

  static var fileURL: URL? {
    FileManager.default.containerURL(forSecurityApplicationGroupIdentifier: appGroup)?.appendingPathComponent(fileName)
  }

  static func decoder() -> JSONDecoder {
    let d = JSONDecoder()
    d.keyDecodingStrategy = .convertFromSnakeCase
    return d
  }

  static func load() -> DailyHealthSnapshot? {
    guard let url = fileURL, let data = try? Data(contentsOf: url) else { return nil }
    return try? decoder().decode(DailyHealthSnapshot.self, from: data)
  }

  /// Writes the raw engine JSON (validated by decoding first); atomic so the widget never reads half a file.
  @discardableResult
  static func save(json data: Data) -> DailyHealthSnapshot? {
    guard let url = fileURL, let snap = try? decoder().decode(DailyHealthSnapshot.self, from: data) else { return nil }
    try? data.write(to: url, options: .atomic)
    return snap
  }
}

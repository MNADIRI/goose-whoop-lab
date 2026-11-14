import SwiftUI
import WidgetKit

// MARK: - Design tokens (subset of FormeTheme.swift; the app file uses UIApplication and cannot be
// compiled into an extension). Fonts are bundled through Info.plist UIAppFonts.

private enum WF {
  static func sans(_ size: CGFloat, _ weight: String = "SemiBold") -> Font { .custom("Manrope-\(weight)", size: size) }
  static func dot(_ size: CGFloat) -> Font { .custom("Doto-ExtraBold", size: size) }
  static func hex(_ v: UInt32, _ a: Double = 1) -> Color {
    Color(red: Double((v >> 16) & 0xff) / 255, green: Double((v >> 8) & 0xff) / 255, blue: Double(v & 0xff) / 255, opacity: a)
  }
  static let ink = hex(0x101618)
  static let ink2 = hex(0x2f3a3c)

  struct Family {
    let edge: Color
    let orbs: [Color]
    let pillInk: Color
  }

  static func family(_ c: DailyContext) -> Family {
    switch c {
    case .morning: return Family(edge: hex(0xe6f6fb), orbs: [hex(0x5fe0b0), hex(0x7cc4ff), hex(0x8fe3e8), hex(0xb8f3dc)], pillInk: hex(0x1d6b47))
    case .daytime: return Family(edge: hex(0xffe8e8), orbs: [hex(0xff5a6a), hex(0xff9aa8), hex(0xffb3bd), hex(0xffd0d6)], pillInk: hex(0x8f1622))
    case .evening, .night: return Family(edge: hex(0xe8f1ff), orbs: [hex(0x7cb0ff), hex(0xa9d8ff), hex(0x9cc3f5), hex(0xd6e6ff)], pillInk: hex(0x24539c))
    }
  }

  static func icon(_ c: DailyContext) -> String {
    switch c {
    case .morning: return "sunrise.fill"
    case .daytime: return "heart.fill"
    case .evening: return "moon.zzz.fill"
    case .night: return "alarm.fill"
    }
  }
}

/// Aura background of the app's coloured cards (pastel field + blurred orbs + frosted sheen).
private struct AuraBackground: View {
  let context: DailyContext
  var body: some View {
    GeometryReader { g in
      let w = g.size.width, h = g.size.height, f = WF.family(context), o = f.orbs
      ZStack {
        f.edge
        Ellipse().fill(o[0]).frame(width: w * 0.82, height: h * 0.74).rotationEffect(.degrees(-18)).offset(x: w * 0.36, y: h * 0.30).blur(radius: 30)
        Ellipse().fill(o[1].opacity(0.85)).frame(width: w * 0.64, height: h * 0.56).rotationEffect(.degrees(24)).offset(x: w * 0.12, y: -h * 0.34).blur(radius: 34)
        Ellipse().fill(o[2].opacity(0.55)).frame(width: w * 0.48, height: h * 0.44).offset(x: -w * 0.32, y: h * 0.22).blur(radius: 36)
        Ellipse().fill(.white.opacity(0.9)).frame(width: w * 0.7, height: h * 0.62).offset(x: -w * 0.32, y: -h * 0.32).blur(radius: 28)
        LinearGradient(colors: [.white.opacity(0.5), .white.opacity(0)], startPoint: .topLeading, endPoint: UnitPoint(x: 0.5, y: 0.5))
      }
      .frame(width: w, height: h)
    }
  }
}

// MARK: - Timeline

struct DailyHealthEntry: TimelineEntry {
  let date: Date
  let card: DailyHealthCard?
  let utcOffset: Int
  /// nil card: no snapshot yet, or older than a day
  let message: String?
}

/// Replays the timeline precomputed by the engine; it computes nothing itself.
struct DailyHealthTimelineProvider: TimelineProvider {
  func placeholder(in context: Context) -> DailyHealthEntry {
    DailyHealthEntry(date: Date(), card: SampleCard.morning, utcOffset: TimeZone.current.secondsFromGMT(), message: nil)
  }

  func getSnapshot(in context: Context, completion: @escaping (DailyHealthEntry) -> Void) {
    if context.isPreview, DailyHealthSnapshotStore.load() == nil {
      completion(placeholder(in: context))
      return
    }
    completion(entries(now: Date()).first ?? placeholder(in: context))
  }

  func getTimeline(in context: Context, completion: @escaping (Timeline<DailyHealthEntry>) -> Void) {
    let now = Date()
    let list = entries(now: now)
    let last = list.last?.date ?? now
    completion(Timeline(entries: list, policy: .after(max(last, now).addingTimeInterval(3600))))
  }

  private func entries(now: Date) -> [DailyHealthEntry] {
    guard let snap = DailyHealthSnapshotStore.load() else {
      return [DailyHealthEntry(date: now, card: nil, utcOffset: TimeZone.current.secondsFromGMT(), message: "Ouvre mn health pour afficher ta journée")]
    }
    if now.timeIntervalSince(snap.generatedDate) > 24 * 3600 {
      return [DailyHealthEntry(date: now, card: nil, utcOffset: snap.utcOffsetSeconds, message: "Données d'hier : ouvre mn health")]
    }
    let t = Int(now.timeIntervalSince1970)
    let current = snap.entries.last { $0.at <= t } ?? snap.entries.first
    var out: [DailyHealthEntry] = []
    if let current {
      out.append(DailyHealthEntry(date: now, card: current.card, utcOffset: snap.utcOffsetSeconds, message: nil))
    }
    for e in snap.entries where e.at > t {
      out.append(DailyHealthEntry(date: Date(timeIntervalSince1970: TimeInterval(e.at)), card: e.card, utcOffset: snap.utcOffsetSeconds, message: nil))
    }
    return out
  }
}

// MARK: - Widget

struct MNHealthDailyWidget: Widget {
  var body: some WidgetConfiguration {
    StaticConfiguration(kind: DailyHealthSnapshotStore.widgetKind, provider: DailyHealthTimelineProvider()) { entry in
      MNHealthWidgetView(entry: entry)
    }
    .configurationDisplayName("mn health")
    .description("Récupération le matin, journée, puis préparation du sommeil le soir.")
    .supportedFamilies([.accessoryRectangular, .accessoryCircular, .accessoryInline, .systemSmall, .systemMedium])
  }
}

struct MNHealthWidgetView: View {
  let entry: DailyHealthEntry
  @Environment(\.widgetFamily) private var family

  var body: some View {
    switch family {
    case .accessoryRectangular: LockRectangular(entry: entry).containerBackground(.clear, for: .widget)
    case .accessoryCircular: LockCircular(entry: entry).containerBackground(.clear, for: .widget)
    case .accessoryInline: LockInline(entry: entry).containerBackground(.clear, for: .widget)
    case .systemMedium: HomeMedium(entry: entry).containerBackground(for: .widget) { AuraBackground(context: entry.card?.context ?? .morning) }
    default: HomeSmall(entry: entry).containerBackground(for: .widget) { AuraBackground(context: entry.card?.context ?? .morning) }
    }
  }
}

// MARK: - Shared formatting

private extension DailyHealthCard {
  /// Unit shown next to the hero number.
  var heroUnit: String {
    switch hero {
    case "recovery": return "/100"
    case "strain": return "%"
    default: return ""
    }
  }

  var heroCaption: String {
    switch context {
    case .morning: return "récupération"
    case .daytime: return "effort"
    case .evening: return "au lit"
    case .night: return "réveil"
    }
  }

  func display(_ id: String) -> String? {
    guard let m = metric(id), !m.isMissing else { return nil }
    return m.display
  }

  /// HR with an honest freshness word: "en direct" only while the engine says it is live.
  var heartLine: String? {
    guard let m = metric("hr"), !m.isMissing else { return nil }
    return "♥ \(m.display)" + (m.live ? " en direct" : (m.detail.map { " \($0)" } ?? ""))
  }

  /// Compact line under the hero (lock screen).
  var compactLine: String {
    let parts: [String?]
    switch context {
    case .morning:
      parts = [display("sleep"), display("hrv").map { "VFC \($0)" }, display("resting_hr").map { "FC \($0)" }]
    case .daytime:
      if let a = metric("activity"), !a.isMissing, a.liveUntil != nil { return a.display }
      parts = [heartLine, display("stress").map { "stress \($0)" }, display("steps").map { "\($0) pas" }]
    case .evening:
      parts = [display("wind_down").map { "décrocher \($0)" }, display("wake").map { "réveil \($0)" }]
    case .night:
      parts = [metric("wake")?.detail, display("sleep_need").map { "besoin \($0)" }]
    }
    let s = parts.compactMap { $0 }.joined(separator: " · ")
    return s.isEmpty ? (heroMetric?.missingReason ?? insight.text) : s
  }

  var heroDetail: String? {
    guard let m = heroMetric else { return nil }
    if m.isMissing { return nil }
    return m.detail
  }
}

private func heroText(_ card: DailyHealthCard) -> String {
  card.heroMetric.flatMap { $0.isMissing ? nil : $0.display } ?? "—"
}

// MARK: - Lock Screen

private struct LockRectangular: View {
  let entry: DailyHealthEntry
  var body: some View {
    if let card = entry.card {
      VStack(alignment: .leading, spacing: 0) {
        HStack(spacing: 4) {
          Image(systemName: WF.icon(card.context)).font(.system(size: 10, weight: .bold))
          Text(card.title.uppercased()).font(WF.sans(11, "Bold")).tracking(0.8).lineLimit(1)
          Spacer(minLength: 0)
          if card.notRefreshedSince != nil { Image(systemName: "clock.badge.exclamationmark").font(.system(size: 10, weight: .semibold)) }
        }
        .widgetAccentable()
        HStack(alignment: .firstTextBaseline, spacing: 4) {
          Text(heroText(card)).font(WF.dot(24)).lineLimit(1).minimumScaleFactor(0.7)
          Text(card.heroUnit.isEmpty ? card.heroCaption : card.heroUnit).font(WF.sans(12, "SemiBold")).opacity(0.8)
        }
        Text(card.compactLine).font(WF.sans(12, "SemiBold")).lineLimit(1).minimumScaleFactor(0.75).opacity(0.9)
      }
      .accessibilityElement(children: .combine)
      .accessibilityLabel("\(card.title), \(heroText(card)) \(card.heroUnit). \(card.insight.text)")
    } else {
      Label(entry.message ?? "mn health", systemImage: "heart.text.square").font(WF.sans(12, "SemiBold"))
    }
  }
}

private struct LockCircular: View {
  let entry: DailyHealthEntry
  var body: some View {
    if let card = entry.card, let m = card.heroMetric {
      switch card.hero {
      case "recovery", "strain":
        let maxV = 100.0
        Gauge(value: min(max(m.value ?? 0, 0), maxV), in: 0...maxV) {
          Image(systemName: WF.icon(card.context))
        } currentValueLabel: {
          Text(m.isMissing ? "—" : m.display).font(WF.sans(15, "Bold")).minimumScaleFactor(0.6)
        }
        .gaugeStyle(.accessoryCircularCapacity)
        .widgetAccentable()
      default:
        ZStack {
          AccessoryWidgetBackground()
          VStack(spacing: 0) {
            Image(systemName: WF.icon(card.context)).font(.system(size: 12, weight: .semibold))
            Text(m.isMissing ? "—" : m.display).font(WF.sans(14, "Bold")).minimumScaleFactor(0.6)
          }
        }
      }
    } else {
      ZStack {
        AccessoryWidgetBackground()
        Image(systemName: "heart.text.square")
      }
    }
  }
}

private struct LockInline: View {
  let entry: DailyHealthEntry
  var body: some View {
    if let card = entry.card {
      let lead: String = {
        switch card.context {
        case .morning: return "Récup \(heroText(card))"
        case .daytime: return "Effort \(heroText(card))"
        case .evening: return "Au lit \(heroText(card))"
        case .night: return "Réveil \(heroText(card))"
        }
      }()
      Label("\(lead) · \(card.compactLine)", systemImage: WF.icon(card.context))
    } else {
      Text(entry.message ?? "mn health")
    }
  }
}

// MARK: - Home Screen (same aura cards as the app)

private struct Pill: View {
  let text: String
  let ink: Color
  var body: some View {
    Text(text.uppercased()).font(WF.sans(10, "Bold")).tracking(0.8).lineLimit(1)
      .foregroundStyle(ink).padding(.horizontal, 8).frame(height: 20)
      .background(.white.opacity(0.66), in: Capsule())
  }
}

private struct HomeSmall: View {
  let entry: DailyHealthEntry
  var body: some View {
    if let card = entry.card {
      VStack(alignment: .leading, spacing: 0) {
        HStack {
          Pill(text: card.title, ink: WF.family(card.context).pillInk)
          Spacer(minLength: 0)
          Image(systemName: WF.icon(card.context)).font(.system(size: 13, weight: .semibold))
        }
        Spacer(minLength: 4)
        HStack(alignment: .firstTextBaseline, spacing: 3) {
          Text(heroText(card)).font(WF.dot(40)).lineLimit(1).minimumScaleFactor(0.6)
          Text(card.heroUnit).font(WF.sans(12, "SemiBold"))
        }
        if let caption = card.heroDetail ?? (card.context == .morning ? nil : card.heroCaption) {
          Text(caption).font(WF.sans(12, "Medium")).foregroundStyle(WF.ink2).lineLimit(1)
        }
        Text(card.compactLine).font(WF.sans(12, "SemiBold")).lineLimit(2).minimumScaleFactor(0.8).padding(.top, 4)
      }
      .foregroundStyle(WF.ink)
    } else {
      EmptyMessage(text: entry.message)
    }
  }
}

private struct HomeMedium: View {
  let entry: DailyHealthEntry
  var body: some View {
    if let card = entry.card {
      VStack(alignment: .leading, spacing: 8) {
        HStack(alignment: .top, spacing: 14) {
          VStack(alignment: .leading, spacing: 2) {
            Pill(text: card.title, ink: WF.family(card.context).pillInk)
            HStack(alignment: .firstTextBaseline, spacing: 3) {
              Text(heroText(card)).font(WF.dot(44)).lineLimit(1).minimumScaleFactor(0.6)
              Text(card.heroUnit).font(WF.sans(12, "SemiBold"))
            }
            .padding(.top, 6)
            if let caption = card.heroDetail ?? (card.context == .morning ? nil : card.heroCaption) {
              Text(caption).font(WF.sans(12, "Medium")).foregroundStyle(WF.ink2).lineLimit(1)
            }
          }
          .layoutPriority(1)
          .frame(maxWidth: .infinity, alignment: .leading)
          VStack(alignment: .leading, spacing: 5) {
            ForEach(card.primary.filter { $0.id != card.hero }.prefix(3)) { m in
              MetricLine(metric: m)
            }
          }
          .frame(maxWidth: .infinity, alignment: .leading)
        }
        Text(card.insight.text).font(WF.sans(12, "SemiBold")).lineLimit(2).minimumScaleFactor(0.85)
          .padding(.horizontal, 10).padding(.vertical, 6)
          .frame(maxWidth: .infinity, alignment: .leading)
          .background(.white.opacity(0.55), in: RoundedRectangle(cornerRadius: 14, style: .continuous))
      }
      .foregroundStyle(WF.ink)
    } else {
      EmptyMessage(text: entry.message)
    }
  }
}

private struct MetricLine: View {
  let metric: HealthMetric
  var body: some View {
    VStack(alignment: .leading, spacing: 0) {
      Text(metric.label).font(WF.sans(11, "SemiBold")).foregroundStyle(WF.ink2).lineLimit(1)
      HStack(alignment: .firstTextBaseline, spacing: 4) {
        Text(metric.isMissing ? "—" : metric.display).font(WF.sans(15, "Bold")).lineLimit(1).minimumScaleFactor(0.7)
        if metric.live {
          Circle().fill(WF.hex(0x1f8a5c)).frame(width: 6, height: 6).accessibilityLabel("en direct")
        } else if let d = metric.isMissing ? nil : metric.detail {
          ViewThatFits(in: .horizontal) {
            Text(d).font(WF.sans(10, "Medium")).foregroundStyle(WF.ink2).lineLimit(1)
            Color.clear.frame(width: 0, height: 0)
          }
        }
      }
    }
  }
}

private struct EmptyMessage: View {
  let text: String?
  var body: some View {
    VStack(alignment: .leading, spacing: 6) {
      Image(systemName: "heart.text.square").font(.system(size: 18, weight: .semibold))
      Text(text ?? "Ouvre mn health").font(WF.sans(13, "SemiBold")).lineLimit(3)
    }
    .foregroundStyle(WF.ink)
    .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
  }
}

// MARK: - Placeholder (widget gallery only; never shown as real data)

private enum SampleCard {
  static func m(_ id: String, _ label: String, _ display: String, _ detail: String? = nil, live: Bool = false) -> HealthMetric {
    HealthMetric(id: id, label: label, value: 1, unit: "", display: display, detail: detail, observedAt: nil, computedAt: nil, validUntil: nil, liveUntil: nil, live: live, source: "aperçu", algorithm: "aperçu", quality: "derived", missingReason: nil, tone: "neutral")
  }

  static let morning = DailyHealthCard(
    context: .morning, title: "Récupération", hero: "recovery",
    primary: [m("recovery", "Récupération", "74"), m("sleep", "Sommeil", "7 h 50", "besoin 8 h 00"), m("hrv", "VFC nuit", "62", "+5 ms vs réf."), m("resting_hr", "Pouls repos", "52", "−1 bpm vs réf.")],
    secondary: [], insight: DailyInsight(id: "recovered", text: "Bien récupéré : bonne journée pour un effort soutenu.", metricIds: ["recovery"]),
    pending: false, notRefreshedSince: nil)
}

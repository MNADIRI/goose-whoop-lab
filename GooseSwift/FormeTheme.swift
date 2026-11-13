import CoreText
import SwiftUI
import UIKit

// MARK: - (MN) design system · "Forme du jour"
//
// Tokens from the (MN) design system v7 (canvas "Forme du jour"): white ground with one faint cool
// light, ink as the only UI accent, two colour families — cool (green→blue: recovery, sleep) and
// red (effort). Coloured cards are frosted glass over blurred orbs of their family. Manrope for
// text, Doto (dot matrix) for hero numbers; both OFL, bundled in Fonts/.

enum Forme {
  // MARK: fonts

  static func registerFonts() {
    guard let urls = Bundle.main.urls(forResourcesWithExtension: "ttf", subdirectory: "Fonts") ?? Bundle.main.urls(forResourcesWithExtension: "ttf", subdirectory: nil) else { return }
    for url in urls {
      CTFontManagerRegisterFontsForURL(url as CFURL, .process, nil)
    }
  }

  enum W { case light, regular, medium, semibold, bold, extrabold }

  static func sans(_ size: CGFloat, _ w: W = .regular, relativeTo style: Font.TextStyle = .body) -> Font {
    let name: String
    switch w {
    case .light: name = "Manrope-Light"
    case .regular: name = "Manrope-Regular"
    case .medium: name = "Manrope-Medium"
    case .semibold: name = "Manrope-SemiBold"
    case .bold: name = "Manrope-Bold"
    case .extrabold: name = "Manrope-ExtraBold"
    }
    return .custom(name, size: size, relativeTo: style)
  }

  /// Dot-matrix hero numbers.
  static func dot(_ size: CGFloat) -> Font { .custom("Doto-ExtraBold", size: size, relativeTo: .largeTitle) }

  // MARK: colours

  static func dyn(_ light: UInt32, _ dark: UInt32, _ la: CGFloat = 1, _ da: CGFloat = 1) -> Color {
    Color(uiColor: UIColor { t in
      let v = t.userInterfaceStyle == .dark ? dark : light
      let a = t.userInterfaceStyle == .dark ? da : la
      return UIColor(red: CGFloat((v >> 16) & 0xff) / 255, green: CGFloat((v >> 8) & 0xff) / 255, blue: CGFloat(v & 0xff) / 255, alpha: a)
    })
  }
  static func hex(_ v: UInt32, _ a: CGFloat = 1) -> Color {
    Color(red: Double((v >> 16) & 0xff) / 255, green: Double((v >> 8) & 0xff) / 255, blue: Double(v & 0xff) / 255, opacity: a)
  }

  static let canvas = dyn(0xf8f9fb, 0x0e1114)
  static let lightPool = dyn(0xdff0f6, 0x111b21)
  static let surface = dyn(0xffffff, 0x191e22)
  static let sunken = dyn(0xeceff3, 0x232a2f)
  static let hairline = dyn(0xdde2e8, 0x2a3137)
  static let dotOff = dyn(0xd8dee5, 0x2b3339)
  static let ink = dyn(0x101618, 0xeef2f2)
  static let ink2 = dyn(0x3d4749, 0xb8c2c3)
  static let muted = dyn(0x545e60, 0x95a0a1)
  static let onBright = hex(0x101618)
  static let onBright2 = hex(0x2f3a3c)
  static let green = dyn(0x1f8a5c, 0x5fd3a0)
  static let blue = dyn(0x2f6fd0, 0x7fb2ff)
  static let strain = dyn(0xd8333f, 0xff6b74)
  static let strainDeep = dyn(0xa51d29, 0xff9aa0)
  static let sleepLight = dyn(0x9cc3f5, 0x3c5f8f)
  static let sleepDeep = dyn(0x1d3a73, 0xe3ecfb)
  static let sleepAwake = dyn(0xd9dee4, 0x3a4249)
  static let posBg = dyn(0xe2f4ea, 0x16332a)
  static let posInk = dyn(0x1d6b47, 0x8fdcb6)
  static let calmBg = dyn(0xe3edfb, 0x1a2a44)
  static let calmInk = dyn(0x24539c, 0xa9c8ff)
  static let strainBg = dyn(0xfbe2e2, 0x3d1c20)
  static let strainInk = dyn(0x9c2128, 0xffa8a3)
  static let selection = dyn(0x101618, 0xeef2f2, 0.07, 0.12)
  static let inset = dyn(0xf3f5f8, 0x20262b)

  /// Appearance setting (Profil › Apparence): "system", "light" or "dark".
  static let appearanceKey = "forme.appearance"
  /// Applied on the windows themselves, so sheets, alerts and UIKit-backed colours follow too.
  @MainActor static func applyAppearance(_ value: String) {
    let style: UIUserInterfaceStyle = value == "light" ? .light : (value == "dark" ? .dark : .unspecified)
    for scene in UIApplication.shared.connectedScenes.compactMap({ $0 as? UIWindowScene }) {
      for window in scene.windows { window.overrideUserInterfaceStyle = style }
    }
  }
  /// Hairline highlight on light frosted chips (white in light, faint in dark).
  static let rim = dyn(0xffffff, 0xffffff, 0.8, 0.08)

  enum Family { case recovery, sleep, effort, calm
    var edge: Color {
      switch self {
      case .recovery, .calm: return Forme.hex(0xe6f6fb)
      case .sleep: return Forme.hex(0xe8f1ff)
      case .effort: return Forme.hex(0xffe8e8)
      }
    }
    var glow: Color {
      switch self {
      case .recovery, .calm: return Forme.hex(0x50c8aa, 0.42)
      case .sleep: return Forme.hex(0x6496f0, 0.40)
      case .effort: return Forme.hex(0xff5a6a, 0.40)
      }
    }
    var orbs: [Color] {
      switch self {
      case .recovery: return [Forme.hex(0x5fe0b0), Forme.hex(0x7cc4ff), Forme.hex(0x8fe3e8), Forme.hex(0xb8f3dc)]
      case .calm: return [Forme.hex(0x7cc4ff), Forme.hex(0x5fe0b0), Forme.hex(0x8fe3e8), Forme.hex(0xb8f3dc)]
      case .sleep: return [Forme.hex(0x7cb0ff), Forme.hex(0xa9d8ff), Forme.hex(0x9cc3f5), Forme.hex(0xd6e6ff)]
      case .effort: return [Forme.hex(0xff5a6a), Forme.hex(0xff9aa8), Forme.hex(0xffb3bd), Forme.hex(0xffd0d6)]
      }
    }
    var pillInk: Color {
      switch self {
      case .recovery, .calm: return Forme.hex(0x1d6b47)
      case .sleep: return Forme.hex(0x24539c)
      case .effort: return Forme.hex(0x8f1622)
      }
    }
    var bar: Color {
      switch self {
      case .recovery, .calm: return Forme.hex(0x1f8a5c)
      case .sleep: return Forme.hex(0x2f6fd0)
      case .effort: return Forme.hex(0xa51d29)
      }
    }
  }
}

// MARK: - Grounds and surfaces

/// Page ground: canvas with one faint cool light pool at the top.
struct FormeGround: View {
  var body: some View {
    ZStack {
      Forme.canvas
      RadialGradient(colors: [Forme.lightPool, Forme.lightPool.opacity(0)], center: UnitPoint(x: 0.5, y: 0), startRadius: 0, endRadius: 520)
        .frame(height: 620)
        .frame(maxHeight: .infinity, alignment: .top)
    }
    .ignoresSafeArea()
  }
}

extension View {
  func formeScreen() -> some View {
    background(FormeGround())
      .toolbar(.hidden, for: .navigationBar)
  }
}

/// Frosted glass card (the default content surface).
struct FrostCard<Content: View>: View {
  var radius: CGFloat = 36
  var padding: CGFloat = 20
  var spacing: CGFloat = 14
  @ViewBuilder var content: Content
  @Environment(\.colorScheme) private var scheme

  var body: some View {
    VStack(alignment: .leading, spacing: spacing) { content }
      .padding(padding)
      .frame(maxWidth: .infinity, alignment: .leading)
      .background {
        RoundedRectangle(cornerRadius: radius, style: .continuous)
          .fill(scheme == .dark
                ? AnyShapeStyle(Color.white.opacity(0.05))
                : AnyShapeStyle(LinearGradient(colors: [.white.opacity(0.92), .white.opacity(0.62), .white.opacity(0.55)], startPoint: .topLeading, endPoint: .bottomTrailing)))
          .background(.ultraThinMaterial, in: RoundedRectangle(cornerRadius: radius, style: .continuous))
          .overlay(RoundedRectangle(cornerRadius: radius, style: .continuous).strokeBorder(.white.opacity(scheme == .dark ? 0.10 : 0.8), lineWidth: 1))
          .shadow(color: Color(red: 40 / 255, green: 60 / 255, blue: 100 / 255).opacity(scheme == .dark ? 0.35 : 0.14), radius: 22, x: 0, y: 14)
      }
  }
}

/// Coloured "aura" card: a pastel field with blurred orbs of its family, frosted sheen on top.
struct AuraCard<Content: View>: View {
  let family: Forme.Family
  var radius: CGFloat = 44
  var padding: CGFloat = 24
  var height: CGFloat?
  @ViewBuilder var content: Content

  var body: some View {
    VStack(alignment: .leading, spacing: 0) { content }
      .padding(padding)
      .frame(maxWidth: .infinity, minHeight: height, maxHeight: height, alignment: .topLeading)
      .foregroundStyle(Forme.onBright)
      .background {
        GeometryReader { g in
          let w = g.size.width, h = g.size.height
          ZStack {
            family.edge
            let o = family.orbs
            Ellipse().fill(o[0]).frame(width: w * 0.82, height: h * 0.74).rotationEffect(.degrees(-18)).offset(x: w * 0.36, y: h * 0.30).blur(radius: 34)
            Ellipse().fill(o[1].opacity(0.85)).frame(width: w * 0.64, height: h * 0.56).rotationEffect(.degrees(24)).offset(x: w * 0.12, y: -h * 0.34).blur(radius: 38)
            Ellipse().fill(o[2].opacity(0.55)).frame(width: w * 0.48, height: h * 0.44).offset(x: -w * 0.32, y: h * 0.22).blur(radius: 40)
            Ellipse().fill(o[3].opacity(0.75)).frame(width: w * 0.34, height: h * 0.26).offset(x: w * 0.22, y: h * 0.06).blur(radius: 20)
            Ellipse().fill(.white.opacity(0.9)).frame(width: w * 0.7, height: h * 0.62).offset(x: -w * 0.32, y: -h * 0.32).blur(radius: 30)
            LinearGradient(colors: [.white.opacity(0.5), .white.opacity(0)], startPoint: .topLeading, endPoint: UnitPoint(x: 0.5, y: 0.5))
          }
          .frame(width: w, height: h)
        }
        .clipShape(RoundedRectangle(cornerRadius: radius, style: .continuous))
        .overlay(RoundedRectangle(cornerRadius: radius, style: .continuous).strokeBorder(.white.opacity(0.75), lineWidth: 1))
        .shadow(color: family.glow, radius: 26, x: 0, y: 20)
      }
      .clipShape(RoundedRectangle(cornerRadius: radius, style: .continuous))
      .contentShape(RoundedRectangle(cornerRadius: radius, style: .continuous))
  }
}

/// Milky panel inside aura cards.
struct VeilPanel<Content: View>: View {
  var radius: CGFloat = 24
  @ViewBuilder var content: Content
  var body: some View {
    content
      .padding(.horizontal, 16)
      .padding(.vertical, 12)
      .frame(maxWidth: .infinity, alignment: .leading)
      .background(.white.opacity(0.55), in: RoundedRectangle(cornerRadius: radius, style: .continuous))
      .overlay(RoundedRectangle(cornerRadius: radius, style: .continuous).strokeBorder(.white.opacity(0.7), lineWidth: 1))
  }
}

/// Liquid glass capsule / circle (chrome: tab bar, back button, chips).
struct GlassBackground: ViewModifier {
  var shape: AnyShape
  @Environment(\.colorScheme) private var scheme
  func body(content: Content) -> some View {
    content
      .background {
        shape.fill(.ultraThinMaterial)
          .overlay(shape.fill(Color.white.opacity(scheme == .dark ? 0.06 : 0.38)))
          .overlay(shape.stroke(Color.white.opacity(scheme == .dark ? 0.18 : 0.95), lineWidth: 1).blendMode(.overlay))
          .overlay(shape.stroke(Forme.ink.opacity(0.06), lineWidth: 1))
          .shadow(color: .black.opacity(scheme == .dark ? 0.45 : 0.12), radius: 18, x: 0, y: 12)
      }
  }
}

extension View {
  func glass<S: Shape>(_ shape: S) -> some View { modifier(GlassBackground(shape: AnyShape(shape))) }
}

// MARK: - Small components

struct FormePill: View {
  let text: String
  var ink: Color = Forme.posInk
  var bg: Color = Forme.posBg
  var body: some View {
    Text(text.uppercased())
      .font(Forme.sans(11, .bold, relativeTo: .caption2))
      .tracking(0.9)
      .lineLimit(1)
      .foregroundStyle(ink)
      .padding(.horizontal, 10)
      .frame(height: 24)
      .background(bg, in: Capsule())
  }
}

/// Pill on aura cards (milky veil).
struct VeilPill: View {
  let text: String
  let ink: Color
  var body: some View { FormePill(text: text, ink: ink, bg: .white.opacity(0.66)) }
}

struct FormeDisc: View {
  let systemImage: String
  var fg: Color = Forme.onBright
  var bg: Color = .white
  var size: CGFloat = 44
  var shadow = true
  var body: some View {
    Image(systemName: systemImage)
      .font(.system(size: size * 0.40, weight: .semibold))
      .foregroundStyle(fg)
      .frame(width: size, height: size)
      .background(bg, in: Circle())
      .shadow(color: .black.opacity(shadow ? 0.10 : 0), radius: 8, x: 0, y: 6)
  }
}

struct SectionTitle: View {
  let title: String
  var subtitle: String?
  var body: some View {
    VStack(alignment: .leading, spacing: 4) {
      Text(title).font(Forme.sans(24, .regular, relativeTo: .title2)).tracking(-0.24).foregroundStyle(Forme.ink)
      if let subtitle { Text(subtitle).font(Forme.sans(12, .medium, relativeTo: .caption)).foregroundStyle(Forme.muted) }
    }
    .frame(maxWidth: .infinity, alignment: .leading)
    .padding(.top, 32)
    .accessibilityElement(children: .combine)
    .accessibilityAddTraits(.isHeader)
  }
}

/// Number with a small unit, e.g. "58 ms".
struct NumberUnit: View {
  let value: String
  var unit: String = ""
  var size: CGFloat = 30
  var color: Color = Forme.ink
  var body: some View {
    HStack(alignment: .firstTextBaseline, spacing: 4) {
      Text(value).font(Forme.sans(size, .semibold, relativeTo: .title)).tracking(-size * 0.02).monospacedDigit()
      if !unit.isEmpty { Text(unit).font(Forme.sans(13, .semibold, relativeTo: .caption)).foregroundStyle(Forme.muted) }
    }
    .foregroundStyle(color)
    .lineLimit(1)
    .minimumScaleFactor(0.6)
  }
}

/// 2-column metric tile on a frosted card (146 pt high).
struct MetricTile: View {
  let label: String
  let value: String
  var unit: String = ""
  var note: String?
  var noteColor: Color = Forme.muted
  var body: some View {
    FrostCard(radius: 32, padding: 18, spacing: 0) {
      Text(label).font(Forme.sans(13, .semibold, relativeTo: .footnote)).foregroundStyle(Forme.ink2)
        .lineLimit(2, reservesSpace: true)
      NumberUnit(value: value, unit: unit).padding(.top, 6)
      Spacer(minLength: 4)
      if let note { Text(note).font(Forme.sans(12, .semibold, relativeTo: .caption)).foregroundStyle(noteColor).lineLimit(1).minimumScaleFactor(0.8) }
    }
    .frame(height: 150)
    .accessibilityElement(children: .combine)
  }
}

/// Small stat: caption over a number.
struct MiniStat: View {
  let label: String
  let value: String
  var unit: String = ""
  var labelColor: Color = Forme.muted
  var size: CGFloat = 20
  var body: some View {
    VStack(alignment: .leading, spacing: 2) {
      Text(label).font(Forme.sans(12, .semibold, relativeTo: .caption)).foregroundStyle(labelColor).lineLimit(1).minimumScaleFactor(0.8)
      NumberUnit(value: value, unit: unit, size: size)
    }
    .frame(maxWidth: .infinity, alignment: .leading)
    .accessibilityElement(children: .combine)
  }
}

/// A row "label / sub / value" with an inner hairline.
struct FormeRow: View {
  let label: String
  var sub: String?
  let value: String
  var tone: Color = Forme.ink
  var body: some View {
    HStack(alignment: .center, spacing: 12) {
      VStack(alignment: .leading, spacing: 1) {
        Text(label).font(Forme.sans(13, .semibold, relativeTo: .footnote)).foregroundStyle(Forme.ink)
        if let sub { Text(sub).font(Forme.sans(12, .medium, relativeTo: .caption)).foregroundStyle(Forme.muted).fixedSize(horizontal: false, vertical: true) }
      }
      Spacer(minLength: 8)
      Text(value).font(Forme.sans(15, .semibold, relativeTo: .subheadline)).monospacedDigit().foregroundStyle(tone).multilineTextAlignment(.trailing)
    }
    .padding(.vertical, 12)
    .accessibilityElement(children: .combine)
  }
}

struct FormeDivider: View {
  var body: some View { Rectangle().fill(Forme.hairline.opacity(0.7)).frame(height: 1) }
}

/// Collapsible "Détails techniques".
struct TechDetails: View {
  let subtitle: String
  let rows: [(String, String, String)]
  @State private var open = false
  var body: some View {
    FrostCard(radius: 36, padding: 20, spacing: 0) {
      Button {
        withAnimation(.snappy(duration: 0.28)) { open.toggle() }
      } label: {
        HStack(spacing: 12) {
          VStack(alignment: .leading, spacing: 1) {
            Text("Détails techniques").font(Forme.sans(15, .bold, relativeTo: .subheadline)).foregroundStyle(Forme.ink)
            Text(subtitle).font(Forme.sans(12, .medium, relativeTo: .caption)).foregroundStyle(Forme.muted)
          }
          Spacer()
          Image(systemName: "chevron.down").font(.system(size: 14, weight: .semibold)).foregroundStyle(Forme.ink)
            .frame(width: 32, height: 32).background(Forme.sunken, in: Circle())
            .rotationEffect(.degrees(open ? 180 : 0))
        }
        .frame(minHeight: 44)
        .contentShape(Rectangle())
      }
      .buttonStyle(.plain)
      .accessibilityValue(open ? "ouvert" : "fermé")
      if open {
        VStack(spacing: 0) {
          ForEach(Array(rows.enumerated()), id: \.offset) { i, r in
            if i > 0 { FormeDivider() }
            FormeRow(label: r.0, sub: r.1.isEmpty ? nil : r.1, value: r.2)
          }
        }
        .padding(.top, 8)
        .transition(.opacity.combined(with: .move(edge: .top)))
      }
    }
  }
}

/// Header of a detail screen: back disc, centred title, chip.
struct DetailHeader: View {
  let title: String
  var chip: String?
  var showsBack = true
  @Environment(\.dismiss) private var dismiss
  var body: some View {
    ZStack {
      Text(title).font(Forme.sans(17, .semibold, relativeTo: .headline)).foregroundStyle(Forme.ink).accessibilityAddTraits(.isHeader)
      HStack {
        if showsBack {
          Button { dismiss() } label: {
            Image(systemName: "chevron.left").font(.system(size: 17, weight: .semibold)).foregroundStyle(Forme.ink)
              .frame(width: 44, height: 44).glass(Circle())
          }
          .buttonStyle(.plain)
          .accessibilityLabel("Retour")
        }
        Spacer()
        if let chip {
          Text(chip).font(Forme.sans(13, .semibold, relativeTo: .footnote)).foregroundStyle(Forme.ink).lineLimit(1)
            .padding(.horizontal, 14).frame(height: 44).glass(Capsule())
        }
      }
    }
    .frame(height: 44)
  }
}

/// Period picker 7 j / 30 j / 90 j on glass.
struct PeriodPicker: View {
  @Binding var period: Int
  var options: [Int] = [7, 30, 90]
  var body: some View {
    HStack(spacing: 0) {
      ForEach(options, id: \.self) { p in
        Button { withAnimation(.snappy(duration: 0.25)) { period = p } } label: {
          Text("\(p) j").font(Forme.sans(13, .bold, relativeTo: .footnote)).foregroundStyle(Forme.ink)
            .frame(minWidth: 52, minHeight: 36).padding(.horizontal, 4)
            .background(period == p ? Forme.selection : .clear, in: Capsule())
        }
        .buttonStyle(.plain)
        .accessibilityAddTraits(period == p ? .isSelected : [])
      }
    }
    .padding(4)
    .glass(Capsule())
  }
}

/// Hero number in the dot-matrix face; a missing value is a quiet sans dash (Doto's dash is a block).
struct DotNumber: View {
  let value: String?
  let size: CGFloat
  var unit: String?
  var unitSize: CGFloat = 26
  var body: some View {
    HStack(alignment: .firstTextBaseline, spacing: 4) {
      if let value {
        Text(value).font(Forme.dot(size)).tracking(-size * 0.02)
        if let unit { Text(unit).font(Forme.sans(unitSize, .semibold)) }
      } else {
        Text("—").font(Forme.sans(size * 0.62, .light)).opacity(0.55)
      }
    }
    .lineLimit(1)
    .minimumScaleFactor(0.6)
  }
}

/// Scroll container used by every Forme screen: 16 pt gutters, max width for iPad, bottom room for
/// the floating tab bar.
struct FormeScroll<Content: View>: View {
  @ViewBuilder var content: Content
  var body: some View {
    ScrollView {
      VStack(alignment: .leading, spacing: 0) { content }
        .padding(.horizontal, 16)
        .padding(.top, 8)
        .padding(.bottom, 120)
        .frame(maxWidth: 560)
        .frame(maxWidth: .infinity)
    }
    .scrollIndicators(.hidden)
    .overlay { EdgeVeil(edge: .top, extra: 18) }
  }
}

/// Soft frosted fade so scrolled content never collides with the status bar / floating tab bar.
struct EdgeVeil: View {
  enum Side { case top, bottom }
  let edge: Side
  var extra: CGFloat
  var body: some View {
    GeometryReader { g in
      let inset = edge == .top ? g.safeAreaInsets.top : g.safeAreaInsets.bottom
      let h = inset + extra
      let solid = h > 0 ? min(0.85, inset / h * 0.8) : 0
      ZStack {
        Rectangle().fill(.ultraThinMaterial)
        Forme.canvas.opacity(0.55)
      }
      .mask(
        LinearGradient(
          stops: [.init(color: .black, location: 0), .init(color: .black, location: solid), .init(color: .clear, location: 1)],
          startPoint: edge == .top ? .top : .bottom,
          endPoint: edge == .top ? .bottom : .top)
      )
      .frame(height: h)
      .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: edge == .top ? .top : .bottom)
      .ignoresSafeArea()
    }
    .allowsHitTesting(false)
  }
}

// MARK: - Charts

/// Dot-matrix week chart: one column of 10 dots per day.
struct DotWeekChart: View {
  struct Col: Identifiable { let id = UUID(); let label: String; let value: Double?; let today: Bool }
  let cols: [Col]
  var maxValue: Double = 100
  var lit: Color = Forme.ink
  var todayLit: Color = Forme.blue
  var body: some View {
    HStack(alignment: .bottom, spacing: 6) {
      ForEach(cols) { c in
        VStack(spacing: 6) {
          let n = Int(((c.value ?? 0) / maxValue * 10).rounded())
          ForEach(0..<10, id: \.self) { k in
            Circle().fill((9 - k) < n ? (c.today ? todayLit : lit) : Forme.dotOff).frame(width: 8, height: 8)
          }
          Text(c.label).font(Forme.sans(12, c.today ? .extrabold : .semibold, relativeTo: .caption)).foregroundStyle(c.today ? Forme.ink : Forme.muted).padding(.top, 4)
        }
        .frame(maxWidth: .infinity)
      }
    }
    .accessibilityElement(children: .ignore)
  }
}

/// Sparkline with dashed mean and the last point marked.
struct Sparkline: View {
  let values: [Double]
  var color: Color = Forme.green
  var lo: Double?
  var hi: Double?
  var body: some View {
    GeometryReader { g in
      let v = values
      let mn = lo ?? (v.min() ?? 0), mx = hi ?? (v.max() ?? 1)
      let span = max(mx - mn, 1e-6)
      let w = g.size.width, h = g.size.height
      // 6 pt inset so the end dot never spills out of the chart
      let pt: (Int) -> CGPoint = { i in CGPoint(x: v.count > 1 ? 6 + CGFloat(i) * (w - 12) / CGFloat(v.count - 1) : w - 6, y: h - 6 - CGFloat((v[i] - mn) / span) * (h - 12)) }
      let avg = v.isEmpty ? 0 : v.reduce(0, +) / Double(v.count)
      let ay = h - 6 - CGFloat((avg - mn) / span) * (h - 12)
      Path { p in p.move(to: CGPoint(x: 0, y: ay)); p.addLine(to: CGPoint(x: w, y: ay)) }
        .stroke(Forme.hairline, style: StrokeStyle(lineWidth: 1, dash: [3, 4]))
      if v.count > 1 {
        Path { p in
          p.move(to: pt(0))
          for i in 1..<v.count { p.addLine(to: pt(i)) }
        }
        .stroke(color, style: StrokeStyle(lineWidth: 2.2, lineCap: .round, lineJoin: .round))
      }
      if let last = v.indices.last {
        Circle().fill(Forme.blue).frame(width: 10, height: 10).overlay(Circle().stroke(.white, lineWidth: 2.5)).position(pt(last))
      }
    }
  }
}

/// Proportional segments (sleep phases, zones).
struct SegmentBar: View {
  let parts: [(Double, Color)]
  var height: CGFloat = 8
  var gap: CGFloat = 3
  var body: some View {
    GeometryReader { g in
      let total = max(parts.map(\.0).reduce(0, +), 1e-9)
      let avail = g.size.width - gap * CGFloat(max(parts.count - 1, 0))
      HStack(spacing: gap) {
        ForEach(Array(parts.enumerated()), id: \.offset) { _, p in
          Capsule().fill(p.1).frame(width: max(height * 0.75, avail * CGFloat(p.0 / total)))
        }
      }
    }
    .frame(height: height)
  }
}

/// Horizontal progress track.
struct Track: View {
  let fraction: Double
  var fill: Color = Forme.ink
  var track: Color = Forme.sunken
  var height: CGFloat = 8
  var body: some View {
    GeometryReader { g in
      ZStack(alignment: .leading) {
        Capsule().fill(track)
        Capsule().fill(fill).frame(width: max(height, g.size.width * min(max(fraction, 0), 1)))
      }
    }
    .frame(height: height)
  }
}

// MARK: - Formatting helpers

enum FF {
  static func num(_ v: Double?, _ d: Int = 0) -> String {
    guard let v, v.isFinite else { return "—" }
    return v.formatted(.number.precision(.fractionLength(d)).locale(Locale(identifier: "fr_FR")))
  }
  static func signed(_ v: Double?, _ d: Int = 0) -> String {
    guard let v, v.isFinite else { return "—" }
    let s = num(abs(v), d)
    let p = pow(10, Double(d))
    let r = (v * p).rounded() / p  // sign of the displayed value, so −0,04 never shows as "−0"
    return r > 0 ? "+\(s)" : (r < 0 ? "−\(s)" : s)
  }
  static func dur(_ minutes: Double?) -> String {
    guard let m = minutes, m.isFinite else { return "—" }
    let t = Int(m.rounded())
    return t >= 60 ? "\(t / 60) h \(String(format: "%02d", t % 60))" : "\(t) min"
  }
  static func clock(_ ts: Double?) -> String {
    guard let ts else { return "—" }
    let f = DateFormatter(); f.locale = Locale(identifier: "fr_FR"); f.dateFormat = "HH:mm"
    return f.string(from: Date(timeIntervalSince1970: ts))
  }
  static func clock(_ d: Date?) -> String { clock(d?.timeIntervalSince1970) }
  static func day(_ d: Date, _ format: String) -> String {
    let f = DateFormatter(); f.locale = Locale(identifier: "fr_FR"); f.dateFormat = format
    return f.string(from: d)
  }
}

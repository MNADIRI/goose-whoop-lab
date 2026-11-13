import SwiftUI

/// Level 1 · glance: recovery, effort, sleep, the day's advice and health signals; then the body
/// last night, the day so far and the week.
struct FormeTodayView: View {
  @EnvironmentObject private var model: GooseAppModel
  @EnvironmentObject private var router: AppRouter
  @ObservedObject var labResults: WhoopLabResultsStore
  @ObservedObject var sport: SportController
  @AppStorage(OnboardingStorage.firstName) private var firstName = ""
  @State private var selectedKey: String?

  var body: some View {
    FormeScroll {
      FormeTodayHeader(ble: model.ble)
      if let day = FormeDay(raw: labResults.raw, key: selectedKey) {
        greeting(day)
        dayStrip(day)
        adviceBanner(day)
        recoveryHero(day).padding(.top, 32)
        HStack(spacing: 12) {
          effortCard(day)
          sleepCard(day)
        }
        .padding(.top, 12)
        signalsRow(day).padding(.top, 12)
        bodySection(day)
        daySection(day)
        weekSection(day)
        indicatorsLink.padding(.top, 12)
      } else {
        greeting(nil)
        emptyState.padding(.top, 32)
      }
    }
    .refreshable {
      labResults.refresh(force: true)
      sport.refresh()
    }
    .formeScreen()
    .onAppear {
      labResults.refresh()
      model.smartWake.refreshPlan()
      model.recordUIAction("page.opened", detail: "Aujourd'hui")
    }
  }

  // MARK: header

  private func greeting(_ day: FormeDay?) -> some View {
    VStack(alignment: .leading, spacing: 6) {
      Text(FF.day(day?.date ?? Date(), "EEEE d MMMM").capitalized(with: Locale(identifier: "fr_FR")))
        .font(Forme.sans(13, .semibold, relativeTo: .footnote)).foregroundStyle(Forme.muted)
      (Text("Bonjour") + Text(firstName.isEmpty ? "" : ", ") + Text(firstName).font(Forme.sans(34, .bold, relativeTo: .largeTitle)))
        .font(Forme.sans(34, .light, relativeTo: .largeTitle))
        .tracking(-0.68)
        .foregroundStyle(Forme.ink)
        .lineLimit(2)
        .minimumScaleFactor(0.7)
    }
    .padding(.top, 28)
  }

  private func dayStrip(_ current: FormeDay) -> some View {
    let latestIndex = current.all.count - 1
    let lo = max(0, latestIndex - 6)
    let keys = (lo...latestIndex).compactMap { current.all[$0]["date"].s }
    return HStack(spacing: 6) {
      ForEach(keys, id: \.self) { key in
        let d = FormeDay(raw: labResults.raw, key: key)
        let selected = key == current.key
        Button {
          withAnimation(.snappy(duration: 0.25)) { selectedKey = key == keys.last ? nil : key }
        } label: {
          VStack(spacing: 4) {
            Text(FF.day(labDate(key) ?? Date(), "EEEEE").uppercased()).font(Forme.sans(11, .bold, relativeTo: .caption2)).tracking(0.66).opacity(0.85)
            Text(FF.day(labDate(key) ?? Date(), "d")).font(Forme.sans(17, .semibold, relativeTo: .headline)).monospacedDigit()
            Circle().fill(selected ? Forme.canvas : Forme.recoveryColor(d?.recoveryScore)).frame(width: 6, height: 6)
          }
          .foregroundStyle(selected ? Forme.canvas : Forme.ink)
          .frame(maxWidth: .infinity)
          .frame(height: 64)
          .background {
            RoundedRectangle(cornerRadius: 18, style: .continuous)
              .fill(selected ? AnyShapeStyle(Forme.ink) : AnyShapeStyle(Forme.surface.opacity(0.55)))
              .overlay(RoundedRectangle(cornerRadius: 18, style: .continuous).strokeBorder(Forme.rim.opacity(selected ? 0 : 1), lineWidth: 1))
              .shadow(color: .black.opacity(selected ? 0.25 : 0), radius: 12, x: 0, y: 10)
          }
        }
        .buttonStyle(.plain)
        .accessibilityLabel(labDayLabel(key))
        .accessibilityValue(d?.recoveryScore.map { "Récupération \(Int($0)) %" } ?? "")
        .accessibilityAddTraits(selected ? .isSelected : [])
      }
    }
    .padding(.top, 24)
  }

  private func adviceBanner(_ day: FormeDay) -> some View {
    let a = day.advice
    return NavigationLink(value: FormeRoute.indicators) {
      HStack(spacing: 14) {
        FormeDisc(systemImage: a.icon)
        VStack(alignment: .leading, spacing: 2) {
          Text(a.title).font(Forme.sans(16, .bold, relativeTo: .headline)).foregroundStyle(Forme.onBright).lineLimit(2)
          Text(a.sub).font(Forme.sans(13, .medium, relativeTo: .footnote)).foregroundStyle(Forme.onBright2).lineLimit(2)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        FormeDisc(systemImage: "arrow.up.right")
      }
      .padding(.vertical, 12)
      .padding(.leading, 16)
      .padding(.trailing, 14)
      .frame(minHeight: 76)
      .background(AuraField(family: .calm).clipShape(Capsule()))
      .overlay(Capsule().strokeBorder(.white.opacity(0.75), lineWidth: 1))
      .shadow(color: Forme.Family.recovery.glow.opacity(0.7), radius: 18, x: 0, y: 14)
      .contentShape(Capsule())
    }
    .buttonStyle(FormePressStyle())
    .padding(.top, 24)
    .accessibilityLabel("Conseil du jour : \(a.title). \(a.sub)")
  }

  // MARK: hero cards

  private func recoveryHero(_ day: FormeDay) -> some View {
    NavigationLink(value: FormeRoute.recovery(day.isToday ? nil : day.key)) {
      AuraCard(family: .recovery, radius: 44, padding: 24, height: 336) {
        HStack {
          Text("Récupération").font(Forme.sans(20, .regular, relativeTo: .title3)).tracking(-0.2)
          Spacer()
          VeilPill(text: day.recoveryWord, ink: Forme.Family.recovery.pillInk)
        }
        DotNumber(value: day.recoveryScore.map { FF.num($0) }, size: 88, unit: "%")
          .padding(.top, 22)
        Text(day.recoverySentence).font(Forme.sans(15, .regular, relativeTo: .body)).foregroundStyle(Forme.hex(0x1c2426))
          .frame(maxWidth: 240, alignment: .leading).padding(.top, 14).lineLimit(3)
        Spacer(minLength: 12)
        VeilPanel {
          HStack(alignment: .top, spacing: 12) {
            heroStat("Variabilité cardiaque", FF.num(day.hrv), "ms", day.hrvDelta.map { "\(FF.signed($0)) vs ta moyenne" }, (day.hrvDelta ?? 0) >= 0 ? Forme.posInk : Forme.calmInk)
            heroStat("Pouls au repos", FF.num(day.rhr), "bpm", day.rhrDelta.map { "\(FF.signed($0)) vs ta moyenne" }, (day.rhrDelta ?? 0) <= 0 ? Forme.posInk : Forme.calmInk)
          }
        }
      }
    }
    .buttonStyle(FormePressStyle())
    .accessibilityLabel("Récupération \(day.recoveryScore.map { "\(Int($0)) %" } ?? "en calibration"), \(day.recoveryWord). Voir le détail")
  }

  private func heroStat(_ label: String, _ value: String, _ unit: String, _ delta: String?, _ deltaColor: Color) -> some View {
    VStack(alignment: .leading, spacing: 2) {
      Text(label).font(Forme.sans(12, .semibold, relativeTo: .caption)).foregroundStyle(Forme.onBright2).lineLimit(1).minimumScaleFactor(0.8)
      HStack(alignment: .firstTextBaseline, spacing: 3) {
        Text(value).font(Forme.sans(20, .semibold, relativeTo: .title3)).monospacedDigit()
        Text(unit).font(Forme.sans(12, .semibold, relativeTo: .caption)).foregroundStyle(Forme.onBright2)
      }
      Text(delta ?? "Référence en cours").font(Forme.sans(12, .semibold, relativeTo: .caption)).foregroundStyle(delta == nil ? Forme.onBright2 : deltaColor).lineLimit(1).minimumScaleFactor(0.8)
    }
    .frame(maxWidth: .infinity, alignment: .leading)
  }

  private func effortCard(_ day: FormeDay) -> some View {
    let count = sport.sessions.filter { Calendar.current.isDate($0.start, inSameDayAs: day.date) && $0.labelStatus != "rejected" && $0.status == "ended" }.count
    return NavigationLink(value: FormeRoute.effort(day.isToday ? nil : day.key)) {
      AuraCard(family: .effort, radius: 32, padding: 20, height: 240) {
        Text("Effort").font(Forme.sans(17, .regular, relativeTo: .headline))
        DotNumber(value: day.strain.map { FF.num($0, 1) }, size: 52).padding(.top, 18)
        Text("sur 21 · " + (count == 0 ? "aucune séance" : "\(count) séance\(count > 1 ? "s" : "")")).font(Forme.sans(12, .semibold, relativeTo: .caption)).foregroundStyle(Forme.onBright2).padding(.top, 6)
        Track(fraction: (day.strain ?? 0) / 21, fill: Forme.hex(0xa51d29), track: .white.opacity(0.66)).padding(.top, 14)
        Spacer(minLength: 8)
        VeilPill(text: day.strainWord, ink: Forme.Family.effort.pillInk)
      }
    }
    .buttonStyle(FormePressStyle())
    .accessibilityLabel("Effort \(day.strain.map { FF.num($0, 1) } ?? "—") sur 21, \(day.strainWord). Voir le détail")
  }

  private func sleepCard(_ day: FormeDay) -> some View {
    let need = (model.smartWake.plan["recommended_sleep_min"] as? Double) ?? 480
    let score = day.sleepScore(needMin: need)
    let st = day.stages
    return NavigationLink(value: FormeRoute.sleep(day.isToday ? nil : day.key)) {
      AuraCard(family: .sleep, radius: 32, padding: 20, height: 240) {
        Text("Sommeil").font(Forme.sans(17, .regular, relativeTo: .headline))
        DotNumber(value: score.map { FF.num($0) }, size: 52, unit: "%", unitSize: 18).padding(.top, 18)
        Text(day.hasNight ? "\(FF.dur(day.asleepMin)) · \(FF.num(day.efficiency)) % d'efficacité" : "Pas de nuit mesurée")
          .font(Forme.sans(12, .semibold, relativeTo: .caption)).foregroundStyle(Forme.onBright2).lineLimit(1).minimumScaleFactor(0.75).padding(.top, 6)
        Group {
          if day.hasNight {
            SegmentBar(parts: [(st["light"] ?? 0, Forme.hex(0x9cc3f5)), (st["rem"] ?? 0, Forme.hex(0x2f6fd0)), (st["deep"] ?? 0, Forme.hex(0x1d3a73))])
          } else {
            Capsule().fill(.white.opacity(0.66))
          }
        }
        .frame(height: 8)
        .padding(.top, 14)
        Spacer(minLength: 8)
        VeilPill(text: day.sleepWord(score), ink: Forme.Family.sleep.pillInk)
      }
    }
    .buttonStyle(FormePressStyle())
    .accessibilityLabel("Sommeil \(score.map { "\(Int($0)) %" } ?? "en attente"), \(FF.dur(day.asleepMin)). Voir le détail")
  }

  private func signalsRow(_ day: FormeDay) -> some View {
    let alert = day.warningLevel == "alert" || day.warningLevel == "watch"
    return NavigationLink(value: FormeRoute.indicators) {
      HStack(spacing: 12) {
        FormeDisc(systemImage: alert ? "exclamationmark.shield" : "checkmark.shield", fg: alert ? Forme.strainInk : Forme.posInk, bg: alert ? Forme.strainBg : Forme.posBg, shadow: false)
        VStack(alignment: .leading, spacing: 1) {
          Text("Signaux de santé : \(day.signalsTitle.lowercased(with: Locale(identifier: "fr_FR")))").font(Forme.sans(15, .semibold, relativeTo: .subheadline)).foregroundStyle(Forme.ink).lineLimit(2)
          Text(day.signalsSubtitle).font(Forme.sans(12, .medium, relativeTo: .caption)).foregroundStyle(Forme.muted).lineLimit(2)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        Image(systemName: "chevron.right").font(.system(size: 15, weight: .semibold)).foregroundStyle(Forme.ink)
      }
      .padding(.vertical, 10)
      .padding(.leading, 10)
      .padding(.trailing, 16)
      .background(FrostCapsule())
    }
    .buttonStyle(FormePressStyle())
  }

  // MARK: sections

  @ViewBuilder private func bodySection(_ day: FormeDay) -> some View {
    SectionTitle(title: "Ton corps cette nuit", subtitle: "Comparé à ta référence personnelle")
    LazyVGrid(columns: [GridItem(.flexible(), spacing: 12), GridItem(.flexible(), spacing: 12)], spacing: 12) {
      MetricTile(label: "Variabilité cardiaque (VFC)", value: FF.num(day.hrv), unit: "ms", note: day.hrvDelta.map { "\(FF.signed($0)) vs référence" } ?? "Référence en cours", noteColor: (day.hrvDelta ?? 0) >= 0 ? Forme.posInk : Forme.calmInk)
      MetricTile(label: "Pouls au repos", value: FF.num(day.rhr), unit: "bpm", note: day.rhrDelta.map { "\(FF.signed($0)) vs référence" } ?? "Référence en cours", noteColor: (day.rhrDelta ?? 0) <= 0 ? Forme.posInk : Forme.calmInk)
      MetricTile(label: "Respiration", value: FF.num(day.resp, 1), unit: "/min", note: day.respDelta.map { abs($0) < 1 ? "Dans ta plage" : "\(FF.signed($0, 1)) vs référence" } ?? "Pendant le sommeil")
      MetricTile(label: "Température cutanée", value: day.skinDelta.map { FF.signed($0, 1) } ?? FF.num(day.skin, 1), unit: "°C", note: day.skinDelta == nil ? "Au poignet, cette nuit" : "Écart à ta référence")
      MetricTile(label: "SpO₂ (saturation en oxygène)", value: FF.num(day.spo2), unit: "%", note: day.spo2Min.map { "Minimum \(FF.num($0)) %" })
      MetricTile(label: "Perfusion", value: day.perfusionPct.map { FF.signed($0) } ?? "—", unit: "%", note: "Amplitude du pouls")
    }
    .padding(.top, 12)
  }

  @ViewBuilder private func daySection(_ day: FormeDay) -> some View {
    SectionTitle(title: "Ta journée", subtitle: day.isToday ? "En cours · \(FF.num(day.coverageH, 1).replacingOccurrences(of: ",0", with: "")) h de port aujourd'hui" : "Journée complète")
    NavigationLink(value: FormeRoute.effort(day.isToday ? nil : day.key)) {
      FrostCard(radius: 36, padding: 20, spacing: 16) {
        VStack(alignment: .leading, spacing: 8) {
          HStack(alignment: .firstTextBaseline) {
            NumberUnit(value: FF.num(day.steps.map(Double.init)), unit: "pas")
            Spacer()
            Text("objectif 8 000").font(Forme.sans(12, .medium, relativeTo: .caption)).foregroundStyle(Forme.muted)
          }
          Track(fraction: Double(day.steps ?? 0) / 8000, fill: (day.steps ?? 0) >= 8000 ? Forme.green : Forme.ink)
        }
        HStack(alignment: .top, spacing: 8) {
          MiniStat(label: "Calories actives", value: FF.num(day.activeKcal), unit: "kcal")
          MiniStat(label: "Minutes actives", value: FF.num(day.mvpa), unit: "min")
          MiniStat(label: "Stress", value: FF.num(day.stressMean), unit: "/100")
        }
      }
    }
    .buttonStyle(FormePressStyle())
    .padding(.top, 12)
  }

  @ViewBuilder private func weekSection(_ day: FormeDay) -> some View {
    let week = day.past(7)
    let values = week.map { $0.recoveryScore }
    let known = values.compactMap { $0 }
    let avg = known.isEmpty ? nil : known.reduce(0, +) / Double(known.count)
    SectionTitle(title: "Ta semaine", subtitle: "Récupération des 7 derniers jours")
    FrostCard(radius: 36, padding: 20, spacing: 16) {
      HStack(alignment: .firstTextBaseline) {
        Text(trend(known)).font(Forme.sans(15, .regular, relativeTo: .body)).foregroundStyle(Forme.ink2)
        Spacer()
        Text("Moy. \(FF.num(avg))").font(Forme.sans(13, .semibold, relativeTo: .footnote)).foregroundStyle(Forme.ink)
      }
      DotWeekChart(cols: week.map { .init(label: FF.day($0.date, "EEEEE").uppercased(), value: $0.recoveryScore, today: $0.key == day.key) })
        .accessibilityLabel("Récupération : " + week.map { "\(FF.day($0.date, "EEEE")) \($0.recoveryScore.map { FF.num($0) } ?? "sans score")" }.joined(separator: ", "))
    }
    .padding(.top, 12)
  }

  private func trend(_ v: [Double]) -> String {
    guard v.count >= 2, let first = v.first, let last = v.last else { return v.isEmpty ? "Score en calibration" : "Première mesure de la semaine" }
    let d = last - first
    if abs(d) < 3 { return "Stable cette semaine" }
    return d > 0 ? "En hausse de \(Int(d)) points" : "En baisse de \(Int(-d)) points"
  }

  private var indicatorsLink: some View {
    NavigationLink(value: FormeRoute.indicators) {
      HStack(spacing: 14) {
        FormeDisc(systemImage: "gauge.with.needle", fg: Forme.ink, bg: Forme.sunken, shadow: false)
        VStack(alignment: .leading, spacing: 2) {
          Text("Indicateurs avancés").font(Forme.sans(15, .semibold, relativeTo: .subheadline)).foregroundStyle(Forme.ink)
          Text("Fatigue, VO₂max, rythme circadien, charge").font(Forme.sans(12, .medium, relativeTo: .caption)).foregroundStyle(Forme.muted)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        Image(systemName: "chevron.right").font(.system(size: 15, weight: .semibold)).foregroundStyle(Forme.ink).frame(width: 44, height: 44)
      }
      .padding(12)
      .background(FrostCapsule())
    }
    .buttonStyle(FormePressStyle())
  }

  private var emptyState: some View {
    FrostCard(radius: 36, padding: 24, spacing: 10) {
      FormeDisc(systemImage: labResults.isRunning ? "hourglass" : "waveform.path.ecg", fg: Forme.ink, bg: Forme.sunken, shadow: false)
      Text(labResults.isRunning ? "Analyse en cours…" : "Pas encore de données").font(Forme.sans(20, .semibold, relativeTo: .title3)).foregroundStyle(Forme.ink)
      Text("Porte le bracelet et laisse la synchronisation se faire. \(labResults.status)").font(Forme.sans(15, .regular, relativeTo: .body)).foregroundStyle(Forme.ink2)
    }
  }
}

/// Header: profile and the device pill (connection, live pulse, battery, last sync).
struct FormeTodayHeader: View {
  @EnvironmentObject private var router: AppRouter
  @ObservedObject var ble: GooseBLEClient

  private var connected: Bool { ["ready", "connected"].contains(ble.connectionState.lowercased()) }

  var body: some View {
    HStack {
      Button { router.openMore(nil) } label: {
        Image(systemName: "person").font(.system(size: 17, weight: .semibold)).foregroundStyle(Forme.ink)
          .frame(width: 44, height: 44).glass(Circle())
      }
      .buttonStyle(.plain)
      .accessibilityLabel("Profil et réglages")
      Spacer()
      NavigationLink(value: FormeRoute.indicators) {
        HStack(spacing: 10) {
          Circle().fill(connected ? Forme.green : Forme.strain).frame(width: 8, height: 8)
            .background(Circle().fill((connected ? Forme.green : Forme.strain).opacity(0.18)).frame(width: 14, height: 14))
          if let hr = ble.liveHeartRateBPM {
            Label { Text("\(hr)").monospacedDigit() } icon: { Image(systemName: "heart").font(.system(size: 13, weight: .semibold)) }
              .labelStyle(TightLabel())
          }
          if let b = ble.batteryLevelPercent {
            Label { Text("\(b) %").monospacedDigit() } icon: { Image(systemName: batteryIcon(b)).font(.system(size: 13, weight: .semibold)) }
              .labelStyle(TightLabel())
          }
          Text(syncText)
        }
        .font(Forme.sans(13, .semibold, relativeTo: .footnote))
        .foregroundStyle(Forme.ink)
        .padding(.horizontal, 14)
        .frame(height: 44)
        .glass(Capsule())
      }
      .buttonStyle(.plain)
      .accessibilityLabel(accessibility)
    }
    .frame(height: 44)
  }

  private var syncText: String {
    if !connected { return "Déconnecté" }
    if ble.isHistoricalSyncing { return "Synchro…" }
    return ble.lastHistoricalSyncCompletedAt.map { FF.clock($0) } ?? "Connecté"
  }

  private var accessibility: String {
    var parts = [connected ? "Bracelet connecté" : "Bracelet déconnecté"]
    if let b = ble.batteryLevelPercent { parts.append("batterie \(b) %") }
    if let s = ble.lastHistoricalSyncCompletedAt { parts.append("synchronisé à \(FF.clock(s))") }
    if let hr = ble.liveHeartRateBPM { parts.append("pouls en direct \(hr) bpm") }
    return parts.joined(separator: ", ")
  }

  private func batteryIcon(_ b: Int) -> String {
    b >= 88 ? "battery.100" : (b >= 63 ? "battery.75" : (b >= 38 ? "battery.50" : (b >= 13 ? "battery.25" : "battery.0")))
  }
}

struct TightLabel: LabelStyle {
  func makeBody(configuration: Configuration) -> some View {
    HStack(spacing: 4) { configuration.icon; configuration.title }
  }
}

/// Frosted capsule background (rows that are links).
struct FrostCapsule: View {
  @Environment(\.colorScheme) private var scheme
  var body: some View {
    Capsule()
      .fill(scheme == .dark ? AnyShapeStyle(Color.white.opacity(0.05)) : AnyShapeStyle(LinearGradient(colors: [.white.opacity(0.92), .white.opacity(0.6)], startPoint: .topLeading, endPoint: .bottomTrailing)))
      .background(.ultraThinMaterial, in: Capsule())
      .overlay(Capsule().strokeBorder(.white.opacity(scheme == .dark ? 0.1 : 0.8), lineWidth: 1))
      .shadow(color: Color(red: 40 / 255, green: 60 / 255, blue: 100 / 255).opacity(scheme == .dark ? 0.3 : 0.12), radius: 18, x: 0, y: 12)
  }
}

/// Just the coloured field of an aura card (for non-rectangular shapes).
struct AuraField: View {
  let family: Forme.Family
  var body: some View {
    GeometryReader { g in
      let w = g.size.width, h = g.size.height, o = family.orbs
      ZStack {
        family.edge
        Ellipse().fill(o[0]).frame(width: w * 0.46, height: h * 1.6).offset(x: w * 0.34, y: -h * 0.2).blur(radius: 26)
        Ellipse().fill(o[1].opacity(0.7)).frame(width: w * 0.42, height: h * 1.4).offset(x: -w * 0.05, y: h * 0.6).blur(radius: 26)
        LinearGradient(colors: [.white.opacity(0.45), .white.opacity(0)], startPoint: .topLeading, endPoint: .center)
      }
      .frame(width: w, height: h)
    }
  }
}

/// Subtle press feedback for card links.
struct FormePressStyle: ButtonStyle {
  func makeBody(configuration: Configuration) -> some View {
    configuration.label
      .scaleEffect(configuration.isPressed ? 0.98 : 1)
      .animation(.snappy(duration: 0.18), value: configuration.isPressed)
  }
}

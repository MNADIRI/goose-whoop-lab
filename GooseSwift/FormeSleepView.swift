import SwiftUI

/// Level 2 · Sleep: score, timing, smart wake and tonight's advice, phases, quality, breathing.
struct FormeSleepView: View {
  @ObservedObject var labResults: WhoopLabResultsStore
  @ObservedObject var smartWake: SmartWakeController
  let dayKey: String?
  let isRoot: Bool

  /// Tonight's recommendation (need + debt payback) from the smart-wake plan.
  private var tonightNeed: Double { (smartWake.plan["recommended_sleep_min"] as? Double) ?? 480 }

  var body: some View {
    FormeScroll {
      let day = FormeDay(raw: labResults.raw, key: dayKey ?? FormeDay.latestNightKey(labResults.raw))
      DetailHeader(title: "Sommeil", chip: day.map(nightChip), showsBack: !isRoot)
      if let day {
        hero(day).padding(.top, 24)
        if day.hasNight { scoreParts(day) }
        smartWakeCard.padding(.top, 12)
        if day.hasNight {
          phases(day)
          quality(day)
          bodyTonight(day)
          TechDetails(subtitle: "Couplage cardio-respiratoire, spectre, horaires", rows: tech(day)).padding(.top, 32)
        }
      } else {
        smartWakeCard.padding(.top, 24)
        FormeEmpty(status: labResults.status).padding(.top, 12)
      }
    }
    .refreshable { labResults.refresh(force: true); smartWake.refreshPlan() }
    .formeScreen()
    .onAppear { smartWake.refreshPlan() }
  }

  private func hero(_ day: FormeDay) -> some View {
    let score = day.sleepScore
    return AuraCard(family: .sleep, radius: 44, padding: 24, height: 330) {
      HStack {
        Text("Performance").font(Forme.sans(20, .regular, relativeTo: .title3)).tracking(-0.2)
        Spacer()
        VeilPill(text: day.sleepWord(score), ink: Forme.Family.sleep.pillInk)
      }
      DotNumber(value: score.map { FF.num($0) }, size: 88, unit: "%").padding(.top, 22)
      Text(day.hasNight ? "\(FF.dur(day.asleepMin)) dormies pour un besoin de \(FF.dur(day.sleepNeed))." : "Aucune nuit mesurée pour ce jour.")
        .font(Forme.sans(15, .regular, relativeTo: .body)).foregroundStyle(Forme.hex(0x1c2426)).padding(.top, 14)
      Spacer(minLength: 12)
      VeilPanel {
        HStack(spacing: 8) {
          clock("Couché", day.bedTime)
          clock("Endormi", day.onset)
          clock("Réveil", day.wake)
        }
      }
    }
    .accessibilityElement(children: .combine)
  }

  private func clock(_ label: String, _ ts: Double?) -> some View {
    VStack(alignment: .leading, spacing: 2) {
      Text(label).font(Forme.sans(12, .semibold, relativeTo: .caption)).foregroundStyle(Forme.onBright2)
      Text(FF.clock(ts)).font(Forme.sans(20, .semibold, relativeTo: .title3)).monospacedDigit()
    }
    .frame(maxWidth: .infinity, alignment: .leading)
  }

  // MARK: smart wake + tonight

  private var smartWakeCard: some View {
    NavigationLink(value: FormeRoute.smartWake) {
      FrostCard(radius: 36, padding: 20, spacing: 14) {
        HStack(spacing: 12) {
          FormeDisc(systemImage: "alarm", fg: Forme.calmInk, bg: Forme.calmBg, shadow: false)
          VStack(alignment: .leading, spacing: 1) {
            Text("Réveil intelligent").font(Forme.sans(15, .bold, relativeTo: .subheadline)).foregroundStyle(Forme.ink)
            Text(smartWake.session != nil ? "Au plus tard \(FF.clock(smartWake.deadline))" : "Désactivé · touche pour régler")
              .font(Forme.sans(12, .medium, relativeTo: .caption)).foregroundStyle(Forme.muted)
          }
          .frame(maxWidth: .infinity, alignment: .leading)
          FormePill(text: smartWake.session != nil ? "Armé" : "Off", ink: smartWake.session != nil ? Forme.calmInk : Forme.ink2, bg: smartWake.session != nil ? Forme.calmBg : Forme.sunken)
        }
        if let ws = smartWake.windowStart, smartWake.session != nil {
          Text("Fenêtre \(FF.clock(ws)) – \(FF.clock(smartWake.deadline)) · \(smartWake.status)")
            .font(Forme.sans(15, .regular, relativeTo: .body)).foregroundStyle(Forme.ink2)
        }
        tonight
      }
    }
    .buttonStyle(FormePressStyle())
  }

  private var tonight: some View {
    let lights = (smartWake.plan["lights_out_advice"] as? Double) ?? (smartWake.plan["lights_out_advice"] as? Int).map(Double.init)
    let comps = smartWake.plan["components"] as? [String: Any] ?? [:]
    let debt = (comps["debt_7d_min"] as? Double) ?? (comps["debt_7d_min"] as? Int).map(Double.init)
    return VStack(alignment: .leading, spacing: 4) {
      Text("Ce soir").font(Forme.sans(13, .semibold, relativeTo: .footnote)).foregroundStyle(Forme.ink2)
      Text(lights.map { "Lumière éteinte vers \(FF.clock($0))" } ?? "Règle ton heure de réveil pour un conseil").font(Forme.sans(17, .semibold, relativeTo: .headline)).foregroundStyle(Forme.ink)
      Text("Besoin de \(FF.dur(tonightNeed))" + (debt.map { $0 >= 10 ? " · dette de \(FF.dur($0)) à rattraper" : " · pas de dette" } ?? ""))
        .font(Forme.sans(12, .medium, relativeTo: .caption)).foregroundStyle(Forme.muted)
    }
    .padding(.horizontal, 16)
    .padding(.vertical, 14)
    .frame(maxWidth: .infinity, alignment: .leading)
    .background(Forme.inset, in: RoundedRectangle(cornerRadius: 22, style: .continuous))
  }

  // MARK: phases

  @ViewBuilder private func phases(_ day: FormeDay) -> some View {
    let st: [String: Double] = day.stages
    let light: Double = st["light"] ?? 0
    let deep: Double = st["deep"] ?? 0
    let rem: Double = st["rem"] ?? 0
    let asleep: Double = max(1, light + deep + rem)
    let t0: Double = day.hypnogramStart ?? day.onset ?? 0
    SectionTitle(title: "Phases", subtitle: "Estimées à partir du cœur et du mouvement")
    FrostCard(radius: 36, padding: 20, spacing: 16) {
      if let h = day.hypnogram, !h.isEmpty {
        Hypnogram(code: h)
          .frame(height: 132)
          .accessibilityLabel("Hypnogramme de la nuit")
        HStack {
          Text(FF.clock(t0))
          Spacer()
          Text(FF.clock(t0 + Double(h.count) * 15))
          Spacer()
          Text(FF.clock(day.wake))
        }
        .font(Forme.sans(12, .medium, relativeTo: .caption)).foregroundStyle(Forme.muted)
      }
      if let unknown = day.night["unknown_min"].d, unknown > 0 {
        Text("Données insuffisantes : \(FF.dur(unknown)). Les interruptions du graphique ne sont pas du sommeil.")
          .font(Forme.sans(12, .medium, relativeTo: .caption)).foregroundStyle(Forme.muted)
      }
      VStack(spacing: 0) {
        ForEach(Array(Self.phaseRows.enumerated()), id: \.offset) { i, p in
          if i > 0 { FormeDivider() }
          PhaseRow(name: p.name, sub: p.sub, color: p.color, minutes: st[p.key], pct: p.key == "wake" ? nil : (st[p.key] ?? 0) / asleep * 100)
        }
      }
    }
    .padding(.top, 12)
  }

  struct PhaseInfo { let name: String; let sub: String; let key: String; let color: Color }
  static let phaseRows: [PhaseInfo] = [
    PhaseInfo(name: "Éveil", sub: "Réveils brefs pendant la nuit", key: "wake", color: Forme.sleepAwake),
    PhaseInfo(name: "Léger", sub: "Transition, la majorité de la nuit", key: "light", color: Forme.sleepLight),
    PhaseInfo(name: "Paradoxal", sub: "Rêves, mémoire, émotions", key: "rem", color: Forme.blue),
    PhaseInfo(name: "Profond", sub: "Récupération physique", key: "deep", color: Forme.sleepDeep),
  ]

  @ViewBuilder private func quality(_ day: FormeDay) -> some View {
    SectionTitle(title: "Qualité", subtitle: "Continuité et régularité de la nuit")
    LazyVGrid(columns: [GridItem(.flexible(), spacing: 12), GridItem(.flexible(), spacing: 12)], spacing: 12) {
      MetricTile(label: "Efficacité", value: FF.num(day.efficiency), unit: "%", note: "Temps endormi au lit")
      MetricTile(label: "Endormissement", value: FF.num(day.latency), unit: "min", note: (day.latency ?? 99) <= 15 ? "Rapide" : ((day.latency ?? 99) <= 30 ? "Normal" : "Lent"), noteColor: (day.latency ?? 99) <= 15 ? Forme.posInk : Forme.muted)
      MetricTile(label: "Réveils", value: day.awakenings.map { "\($0)" } ?? "—", note: day.stages["wake"].map { "\(FF.dur($0)) éveillé au total" })
      MetricTile(label: "Régularité", value: FF.num(day.regularity), unit: "/100", note: "Horaires sur 7 nuits")
    }
    .padding(.top, 12)
  }

  // MARK: score breakdown

  @ViewBuilder private func scoreParts(_ day: FormeDay) -> some View {
    SectionTitle(title: "Ce qui fait ton score", subtitle: "Durée, continuité, régularité et phases, pondérées")
    FrostCard(radius: 36, padding: 20, spacing: 0) {
      ForEach(Array(day.sleepScoreParts.enumerated()), id: \.offset) { i, p in
        if i > 0 { FormeDivider() }
        VStack(alignment: .leading, spacing: 8) {
          HStack(alignment: .firstTextBaseline) {
            Text(p.label).font(Forme.sans(15, .semibold, relativeTo: .subheadline)).foregroundStyle(Forme.ink)
            Text("\(Int((p.weight * 100).rounded())) %").font(Forme.sans(12, .medium, relativeTo: .caption)).foregroundStyle(Forme.muted)
            Spacer()
            Text(p.score.map { FF.num($0) } ?? "—").font(Forme.sans(17, .semibold, relativeTo: .headline)).monospacedDigit().foregroundStyle(Forme.ink)
          }
          Track(fraction: (p.score ?? 0) / 100, fill: p.score == nil ? Forme.dotOff : Forme.blue, track: Forme.sunken)
          Text(p.explain).font(Forme.sans(12, .medium, relativeTo: .caption)).foregroundStyle(Forme.muted).fixedSize(horizontal: false, vertical: true)
        }
        .padding(.vertical, 12)
        .accessibilityElement(children: .combine)
      }
    }
    .padding(.top, 12)
  }

  // MARK: body tonight

  @ViewBuilder private func bodyTonight(_ day: FormeDay) -> some View {
    SectionTitle(title: "Ton corps cette nuit", subtitle: "Mesuré au calme du sommeil, comparé à ta référence")
    LazyVGrid(columns: [GridItem(.flexible(), spacing: 12), GridItem(.flexible(), spacing: 12)], spacing: 12) {
      MetricTile(label: "Variabilité cardiaque (VFC)", value: FF.num(day.hrv), unit: "ms", note: day.hrvDelta.map { "\(FF.signed($0)) vs référence" } ?? "Référence en cours", noteColor: (day.hrvDelta ?? 0) >= 0 ? Forme.posInk : Forme.calmInk)
      MetricTile(label: "Pouls au repos", value: FF.num(day.rhr), unit: "bpm", note: day.rhrDelta.map { "\(FF.signed($0)) vs référence" } ?? "Référence en cours", noteColor: (day.rhrDelta ?? 0) <= 0 ? Forme.posInk : Forme.calmInk)
      MetricTile(label: "Respiration", value: FF.num(day.resp, 1), unit: "/min", note: day.respDelta.map { abs($0) < 1 ? "Dans ta plage" : "\(FF.signed($0, 1)) vs référence" } ?? "Fréquence médiane")
      MetricTile(label: "Température cutanée", value: day.skinDelta.map { FF.signed($0, 1) } ?? FF.num(day.skin, 1), unit: "°C", note: day.skinDelta == nil ? "Au poignet" : "Écart à ta référence")
      MetricTile(label: "SpO₂ (saturation en oxygène)", value: FF.num(day.spo2), unit: "%", note: day.spo2Min.map { "Minimum \(FF.num($0)) %" })
      MetricTile(label: "Perfusion", value: day.perfusionPct.map { FF.signed($0) } ?? "—", unit: "%", note: "Amplitude du pouls vs nuits")
      MetricTile(label: "Accélérations du pouls", value: FF.num(day.hrSurgesPerH, 1), unit: "/h", note: "Descriptif ; ne mesure pas les apnées")
      MetricTile(label: "Mouvements", value: FF.num(day.postureChangesPerH, 1), unit: "/h", note: "Changements de position")
    }
    .padding(.top, 12)
  }

  private func tech(_ day: FormeDay) -> [(String, String, String)] {
    let n = day.night
    return [
      ("LF/HF de la nuit", "Rapport spectral descriptif, sans interprétation autonome", FF.num(n["hrv_spectral"]["night"]["lf_hf"].d, 2)),
      ("Variabilité de la respiration", "Coefficient de variation du rythme", n["breathing_regularity"]["rate_cv_pct"].d.map { "\(FF.num($0, 1)) %" } ?? "—"),
      ("Température cutanée", "Au poignet pendant le sommeil", n["skin_temp_c"].d.map { "\(FF.num($0, 1)) °C" } ?? "—"),
      ("Réchauffement à l'endormissement", "Hausse de température cutanée", n["skin_onset_warming_c"].d.map { "\(FF.signed($0, 2)) °C" } ?? "—"),
      ("Temps au lit", "Du coucher au réveil", FF.dur(n["time_in_bed_min"].d)),
      ("Sommeil détecté par le bracelet", "État « sleep » du bracelet", FF.dur(n["firmware_sleep_min"].d)),
    ]
  }
}

/// Four-lane hypnogram from the 30 s epoch code (W, L, D, R).
struct Hypnogram: View {
  let code: String
  var body: some View {
    let chars = Array(code)
    let lanes: [(String, Character, Color)] = [("Éveil", "W", Forme.sleepAwake), ("Paradoxal", "R", Forme.blue), ("Léger", "L", Forme.sleepLight), ("Profond", "D", Forme.sleepDeep)]
    GeometryReader { g in
      let labelW: CGFloat = 70
      let w = g.size.width - labelW
      let laneH = g.size.height / 4
      ZStack(alignment: .topLeading) {
        ForEach(Array(lanes.enumerated()), id: \.offset) { li, lane in
          Text(lane.0).font(Forme.sans(11, .semibold, relativeTo: .caption2)).foregroundStyle(Forme.muted)
            .frame(width: labelW - 8, height: laneH, alignment: .leading)
            .offset(y: CGFloat(li) * laneH)
          Capsule().fill(Forme.sunken.opacity(0.6)).frame(width: w, height: laneH * 0.55).offset(x: labelW, y: CGFloat(li) * laneH + laneH * 0.225)
          Canvas { ctx, size in
            let n = CGFloat(max(chars.count, 1))
            var i = 0
            while i < chars.count {
              guard chars[i] == lane.1 else { i += 1; continue }
              var j = i
              while j < chars.count && chars[j] == lane.1 { j += 1 }
              let r = CGRect(x: CGFloat(i) / n * size.width, y: 0, width: max(2, CGFloat(j - i) / n * size.width), height: size.height)
              ctx.fill(Path(roundedRect: r, cornerRadius: min(4, r.width / 2)), with: .color(lane.2))
              i = j
            }
          }
          .frame(width: w, height: laneH * 0.55)
          .offset(x: labelW, y: CGFloat(li) * laneH + laneH * 0.225)
        }
      }
    }
  }
}

struct PhaseRow: View {
  let name: String
  let sub: String
  let color: Color
  let minutes: Double?
  let pct: Double?
  var body: some View {
    HStack(spacing: 12) {
      RoundedRectangle(cornerRadius: 4).fill(color).frame(width: 14, height: 14)
      VStack(alignment: .leading, spacing: 1) {
        Text(name).font(Forme.sans(14, .semibold, relativeTo: .subheadline)).foregroundStyle(Forme.ink)
        Text(sub).font(Forme.sans(12, .medium, relativeTo: .caption)).foregroundStyle(Forme.muted)
      }
      Spacer()
      VStack(alignment: .trailing, spacing: 1) {
        Text(FF.dur(minutes)).font(Forme.sans(15, .semibold, relativeTo: .subheadline)).monospacedDigit().foregroundStyle(Forme.ink)
        if let pct { Text("\(FF.num(pct)) %").font(Forme.sans(12, .medium, relativeTo: .caption)).foregroundStyle(Forme.muted) }
      }
    }
    .padding(.vertical, 10)
    .accessibilityElement(children: .combine)
  }
}

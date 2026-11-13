import SwiftUI

/// Level 2 · Effort: day strain, the sessions (one list: detected and started activities, with the
/// ones to review first), movement, hourly pulse and steps, stress at rest, zones, load, week.
struct FormeEffortView: View {
  @ObservedObject var labResults: WhoopLabResultsStore
  @ObservedObject var sport: SportController
  let dayKey: String?
  let isRoot: Bool

  var body: some View {
    FormeScroll {
      let day = FormeDay(raw: labResults.raw, key: dayKey)
      DetailHeader(title: "Effort", chip: day.map { $0.isToday ? "Aujourd'hui" : FF.day($0.date, "EEE d").capitalized }, showsBack: !isRoot)
      if let day {
        hero(day).padding(.top, 24)
        sessions(day)
        movement(day)
        hourly(day)
        stress(day)
        zones(day)
        load(day)
        TechDetails(subtitle: "TRIMP, forme de Banister, monotonie", rows: tech(day)).padding(.top, 32)
      } else {
        sessionsOnly
        FormeEmpty(status: labResults.status).padding(.top, 12)
      }
    }
    .refreshable { labResults.refresh(force: true); sport.refresh() }
    .formeScreen()
    .onAppear { sport.refresh() }
  }

  private func daySessions(_ day: FormeDay) -> [SportSession] {
    sport.sessions.filter { Calendar.current.isDate($0.start, inSameDayAs: day.date) && $0.labelStatus != "rejected" && $0.status == "ended" }
      .sorted { $0.start < $1.start }
  }

  private func hero(_ day: FormeDay) -> some View {
    let n = daySessions(day).count
    let z = day.zones
    let main = z.enumerated().max { $0.element < $1.element }.map { $0.offset + 1 } ?? 1
    return AuraCard(family: .effort, radius: 44, padding: 24, height: 330) {
      HStack {
        Text("Effort du jour").font(Forme.sans(20, .regular, relativeTo: .title3)).tracking(-0.2)
        Spacer()
        VeilPill(text: day.strainWord, ink: Forme.Family.effort.pillInk)
      }
      DotNumber(value: day.strain.map { FF.num($0, 1) }, size: 88, unit: day.strain == nil ? nil : "/ 21", unitSize: 22).padding(.top, 22)
      Text(n == 0 ? "Aucune séance, effort du quotidien surtout en zone \(main)." : "\(n == 1 ? "Une séance" : "\(n) séances"), surtout en zone\(main <= 2 ? "s 1 et 2" : " \(main)").")
        .font(Forme.sans(15, .regular, relativeTo: .body)).foregroundStyle(Forme.hex(0x1c2426)).padding(.top, 14)
      Track(fraction: (day.strain ?? 0) / 21, fill: Forme.hex(0xa51d29), track: .white.opacity(0.66)).padding(.top, 14)
      Spacer(minLength: 12)
      VeilPanel {
        HStack(spacing: 8) {
          MiniStat(label: "Calories", value: FF.num(day.totalKcal), labelColor: Forme.onBright2)
          MiniStat(label: "Pouls moyen", value: FF.num(day.hrMean), unit: "bpm", labelColor: Forme.onBright2)
          MiniStat(label: "Pouls max", value: FF.num(day.hrMax), unit: "bpm", labelColor: Forme.onBright2)
        }
      }
    }
  }

  // MARK: sessions

  private var reviewQueue: [SportSession] { sport.reviewQueue.compactMap { id in sport.sessions.first { $0.id == id } } }

  @ViewBuilder private var sessionsOnly: some View {
    if !reviewQueue.isEmpty {
      SectionTitle(title: "À vérifier", subtitle: "Une touche pour confirmer, corriger ou écarter")
      VStack(spacing: 12) { ForEach(reviewQueue) { s in FormeReviewCard(session: s, sport: sport) } }.padding(.top, 12)
    }
  }

  @ViewBuilder private func sessions(_ day: FormeDay) -> some View {
    let list = daySessions(day)
    let pending = isRoot ? reviewQueue : reviewQueue.filter { Calendar.current.isDate($0.start, inSameDayAs: day.date) }
    if !pending.isEmpty {
      SectionTitle(title: "À vérifier", subtitle: "Une touche pour confirmer, corriger ou écarter")
      VStack(spacing: 12) { ForEach(pending) { s in FormeReviewCard(session: s, sport: sport) } }.padding(.top, 12)
    }
    SectionTitle(title: "Séances", subtitle: "Détectées automatiquement ou lancées par toi · touche pour le détail")
    let shown = list.filter { s in !pending.contains { $0.id == s.id } }
    FrostCard(radius: 36, padding: shown.isEmpty ? 20 : 8, spacing: 0) {
      if shown.isEmpty {
        Text("Aucune séance ce jour. Lance une activité avec le bouton rouge, ou laisse la détection automatique faire.")
          .font(Forme.sans(15, .regular, relativeTo: .body)).foregroundStyle(Forme.ink2)
      }
      ForEach(Array(shown.enumerated()), id: \.element.id) { i, s in
        if i > 0 { FormeDivider().padding(.horizontal, 12) }
        NavigationLink(value: FormeRoute.session(s.id)) { FormeSessionRow(session: s) }
          .buttonStyle(.plain)
          .contextMenu { Button("Supprimer la séance", systemImage: "trash", role: .destructive) { sport.delete(s.id) } }
      }
    }
    .padding(.top, 12)
    if isRoot {
      let pendingIDs = Set(pending.map(\.id))
      let recent = sport.sessions.filter { s in !Calendar.current.isDateInToday(s.start) && s.status == "ended" && s.labelStatus != "rejected" && !pendingIDs.contains(s.id) }.prefix(8)
      if !recent.isEmpty {
        SectionTitle(title: "Séances récentes", subtitle: "Les 60 derniers jours")
        FrostCard(radius: 36, padding: 8, spacing: 0) {
          ForEach(Array(recent.enumerated()), id: \.element.id) { i, s in
            if i > 0 { FormeDivider().padding(.horizontal, 12) }
            NavigationLink(value: FormeRoute.session(s.id)) { FormeSessionRow(session: s, showsDay: true) }
              .buttonStyle(.plain)
              .contextMenu { Button("Supprimer la séance", systemImage: "trash", role: .destructive) { sport.delete(s.id) } }
          }
        }
        .padding(.top, 12)
      }
    }
  }

  // MARK: movement

  @ViewBuilder private func movement(_ day: FormeDay) -> some View {
    SectionTitle(title: "Ta journée", subtitle: "Mouvement et dépense")
    LazyVGrid(columns: [GridItem(.flexible(), spacing: 12), GridItem(.flexible(), spacing: 12)], spacing: 12) {
      MetricTile(label: "Pas", value: FF.num(day.steps.map(Double.init)), note: (day.steps ?? 0) >= 8000 ? "Objectif 8 000 atteint" : "Objectif 8 000", noteColor: (day.steps ?? 0) >= 8000 ? Forme.posInk : Forme.muted)
      MetricTile(label: "Calories actives", value: FF.num(day.activeKcal), unit: "kcal", note: day.totalKcal.map { "Total \(FF.num($0)) kcal" })
      MetricTile(label: "Activité modérée+", value: FF.num(day.mvpa), unit: "min", note: "Repère OMS : 22 min")
      MetricTile(label: "Marche · course", value: "\(FF.num(day.walkMin)) · \(FF.num(day.runMin))", unit: "min", note: day.cadence.map { "Cadence \(FF.num($0)) pas/min" })
      MetricTile(label: "Sédentarité", value: FF.dur(day.sedentaryMin), note: day.longestSedentary.map { "Plus longue période \(FF.dur($0))" })
      MetricTile(label: "Stress", value: FF.num(day.stressMean), unit: "/100", note: day.stressMean.map { "\($0 < 25 ? "Bas" : ($0 < 50 ? "Modéré" : "Élevé")) · max \(FF.num(day.stressMax))" })
    }
    .padding(.top, 12)
  }

  @ViewBuilder private func hourly(_ day: FormeDay) -> some View {
    let h = day.hourly
    if !h.isEmpty {
      let maxSteps = Double(max(h.map(\.steps).max() ?? 1, 1))
      let hrs = h.map(\.hr).filter { $0 > 0 }
      let lo = hrs.min() ?? 40, hi = max(hrs.max() ?? 100, lo + 1)
      FrostCard(radius: 36, padding: 20, spacing: 12) {
        HStack {
          Text("Pouls et pas par heure").font(Forme.sans(13, .semibold, relativeTo: .footnote)).foregroundStyle(Forme.ink2)
          Spacer()
          Text("\(FF.num(lo)) – \(FF.num(hi)) bpm").font(Forme.sans(12, .medium, relativeTo: .caption)).foregroundStyle(Forme.muted)
        }
        GeometryReader { g in
          let w = g.size.width, ht = g.size.height
          let slot = w / 24
          ZStack(alignment: .bottomLeading) {
            ForEach(h, id: \.hour) { x in
              RoundedRectangle(cornerRadius: 3).fill(Forme.sunken)
                .frame(width: slot * 0.62, height: max(3, CGFloat(Double(x.steps) / maxSteps) * ht * 0.9))
                .offset(x: CGFloat(x.hour) * slot + slot * 0.19)
            }
            Path { p in
              var first = true
              for x in h where x.hr > 0 {
                let pt = CGPoint(x: CGFloat(x.hour) * slot + slot / 2, y: ht - CGFloat((x.hr - lo) / (hi - lo)) * ht * 0.85 - ht * 0.05)
                if first { p.move(to: pt); first = false } else { p.addLine(to: pt) }
              }
            }
            .stroke(Forme.strain, style: StrokeStyle(lineWidth: 2, lineCap: .round, lineJoin: .round))
          }
        }
        .frame(height: 110)
        .accessibilityLabel("Pas et pouls par heure")
        HStack { Text("0 h"); Spacer(); Text("12 h"); Spacer(); Text("23 h") }
          .font(Forme.sans(12, .medium, relativeTo: .caption)).foregroundStyle(Forme.muted)
      }
      .padding(.top, 12)
    }
  }

  @ViewBuilder private func stress(_ day: FormeDay) -> some View {
    let m = day.stressMinutes
    if !m.isEmpty {
      let parts: [(String, String, Color)] = [("Repos", "repos", Forme.hex(0xcfe0f7)), ("Bas", "bas", Forme.hex(0x9cc3f5)), ("Moyen", "moyen", Forme.hex(0xf3a6ae)), ("Haut", "haut", Forme.strain)]
      FrostCard(radius: 36, padding: 20, spacing: 14) {
        HStack {
          Text("Stress au repos").font(Forme.sans(13, .semibold, relativeTo: .footnote)).foregroundStyle(Forme.ink2)
          Spacer()
          Text("Éveillé et immobile").font(Forme.sans(12, .medium, relativeTo: .caption)).foregroundStyle(Forme.muted)
        }
        SegmentBar(parts: parts.map { (m[$0.1] ?? 0, $0.2) }, height: 10)
        HStack {
          ForEach(parts, id: \.1) { p in
            VStack(alignment: .leading, spacing: 2) {
              HStack(spacing: 6) { Circle().fill(p.2).frame(width: 8, height: 8); Text(p.0) }
                .font(Forme.sans(12, .medium, relativeTo: .caption)).foregroundStyle(Forme.muted)
              Text(FF.dur(m[p.1])).font(Forme.sans(15, .semibold, relativeTo: .subheadline)).monospacedDigit().foregroundStyle(Forme.ink)
            }
            .frame(maxWidth: .infinity, alignment: .leading)
          }
        }
      }
      .padding(.top, 12)
    }
  }

  @ViewBuilder private func zones(_ day: FormeDay) -> some View {
    let z = day.zones
    let mx = max(z.max() ?? 1, 1)
    let names = ["Zone 1", "Zone 2", "Zone 3", "Zone 4", "Zone 5"]
    let ranges = ["50–60 %", "60–70 %", "70–80 %", "80–90 %", "90–100 %"]
    let colors = [Forme.hex(0xcfe0f7), Forme.hex(0x9cc3f5), Forme.hex(0xf3a6ae), Forme.hex(0xe0606b), Forme.strainDeep]
    SectionTitle(title: "Zones cardiaques", subtitle: "Temps par zone, en % de ta fréquence cardiaque max")
    FrostCard(radius: 36, padding: 20, spacing: 12) {
      ForEach(0..<5, id: \.self) { i in
        HStack(spacing: 12) {
          VStack(alignment: .leading, spacing: 0) {
            Text(names[i]).font(Forme.sans(13, .semibold, relativeTo: .footnote)).foregroundStyle(Forme.ink)
            Text(ranges[i]).font(Forme.sans(11, .medium, relativeTo: .caption2)).foregroundStyle(Forme.muted)
          }
          .frame(width: 72, alignment: .leading)
          Track(fraction: z[i] / mx, fill: colors[i], height: 10)
          Text(FF.dur(z[i])).font(Forme.sans(13, .semibold, relativeTo: .footnote)).monospacedDigit().foregroundStyle(Forme.ink).frame(width: 64, alignment: .trailing)
        }
        .accessibilityElement(children: .combine)
      }
    }
    .padding(.top, 12)
  }

  @ViewBuilder private func load(_ day: FormeDay) -> some View {
    let week = day.past(7)
    SectionTitle(title: "Charge d'entraînement", subtitle: "Effort cumulé sur 7 et 28 jours")
    FrostCard(radius: 36, padding: 20, spacing: 16) {
      HStack(alignment: .top, spacing: 8) {
        MiniStat(label: "7 jours", value: FF.num(day.acute7))
        MiniStat(label: "Moy. hebdo 28 j", value: FF.num(day.chronicWeekly))
        MiniStat(label: "Ratio aigu/chronique", value: day.acwr.map { FF.num($0, 2) } ?? "—")
      }
      if let r = day.acwr {
        VStack(alignment: .leading, spacing: 6) {
          GeometryReader { g in
            let w = g.size.width
            ZStack(alignment: .leading) {
              Capsule().fill(Forme.sunken)
              Capsule().fill(Forme.posBg).frame(width: w * 0.25).offset(x: w * 0.4)
              Circle().fill(.white).frame(width: 16, height: 16).overlay(Circle().strokeBorder(Forme.ink, lineWidth: 3))
                .offset(x: w * CGFloat(min(max(r, 0), 2) / 2) - 8)
            }
          }
          .frame(height: 8)
          HStack { Text("0"); Spacer(); Text("0,8 – 1,3 : charge habituelle"); Spacer(); Text("2") }
            .font(Forme.sans(12, .medium, relativeTo: .caption)).foregroundStyle(Forme.muted)
        }
      } else {
        Text("Le ratio arrive après 21 jours de données.").font(Forme.sans(12, .medium, relativeTo: .caption)).foregroundStyle(Forme.muted)
      }
      DotWeekChart(cols: week.map { .init(label: FF.day($0.date, "EEEEE").uppercased(), value: $0.strain, today: $0.key == day.key) }, maxValue: 21, lit: Forme.ink, todayLit: Forme.strain)
        .accessibilityLabel("Effort : " + week.map { "\(FF.day($0.date, "EEEE")) \(FF.num($0.strain, 1))" }.joined(separator: ", "))
    }
    .padding(.top, 12)
  }

  private func tech(_ day: FormeDay) -> [(String, String, String)] {
    let d = day.day
    let b = day.banister
    return [
      ("TRIMP d'Edwards", "Minutes par zone pondérées 1 à 5", FF.num(d["edwards_trimp"].d, 1)),
      ("TRIMP de Banister", "Réserve cardiaque pondérée exponentiellement", FF.num(d["training"]["load_karvonen"]["banister_trimp"].d, 1)),
      ("Fatigue récente (ATL)", "Charge des 7 derniers jours, lissée", FF.num(b.atl, 1)),
      ("Forme de fond (CTL)", "Charge des 42 derniers jours, lissée", FF.num(b.ctl, 1)),
      ("Fraîcheur (TSB)", "Forme de fond − fatigue récente", FF.signed(b.tsb, 1)),
      ("Monotonie (Foster)", "Moyenne / écart-type de la charge sur 7 j", FF.num(day.root["load"]["foster"]["monotony"].d, 2)),
      ("FC max utilisée", "Max(Tanaka, observée)", d["hrmax_used"].d.map { "\(FF.num($0)) bpm" } ?? "—"),
      ("Port du bracelet", "Couverture des mesures ce jour", day.coverageH.map { "\(FF.num($0, 1)) h" } ?? "—"),
    ]
  }
}

// MARK: - Session rows and review

extension SportKind {
  var family: Forme.Family { self == .course || self == .tennis || self == .musculation ? .effort : .calm }
}

struct FormeSessionRow: View {
  let session: SportSession
  var showsDay = false
  var body: some View {
    let k = SportKind.of(session.type)
    HStack(spacing: 12) {
      Image(systemName: k.icon).font(.system(size: 18, weight: .semibold)).foregroundStyle(Forme.strainInk)
        .frame(width: 44, height: 44).background(Forme.strainBg, in: Circle())
      VStack(alignment: .leading, spacing: 2) {
        HStack(spacing: 6) {
          Text(k.title).font(Forme.sans(15, .semibold, relativeTo: .subheadline)).foregroundStyle(Forme.ink)
          if session.source.contains("manual") { Image(systemName: "hand.tap").font(.system(size: 11, weight: .semibold)).foregroundStyle(Forme.muted).accessibilityLabel("lancée par toi") }
        }
        Text(meta).font(Forme.sans(12, .medium, relativeTo: .caption)).foregroundStyle(Forme.muted).lineLimit(1)
        if let line = session.strengthLine {
          Text(line).font(Forme.sans(12, .semibold, relativeTo: .caption)).foregroundStyle(Forme.strainInk).lineLimit(1)
        } else if let hr = session.hrMean {
          Text("Pouls moyen \(FF.num(hr)) · max \(FF.num(session.hrMax))").font(Forme.sans(12, .semibold, relativeTo: .caption)).foregroundStyle(Forme.ink2).lineLimit(1)
        }
      }
      .frame(maxWidth: .infinity, alignment: .leading)
      if session.isValidated { Image(systemName: "checkmark.seal.fill").foregroundStyle(Forme.green).accessibilityLabel("Validée") }
      Image(systemName: "chevron.right").font(.system(size: 13, weight: .semibold)).foregroundStyle(Forme.muted)
    }
    .padding(12)
    .contentShape(Rectangle())
    .accessibilityElement(children: .combine)
  }

  private var meta: String {
    var s = showsDay ? FF.day(session.start, "EEE d · ").capitalized : ""
    s += FF.clock(session.start)
    if let e = session.end { s += " – \(FF.clock(e))" }
    if let d = session.durationMin { s += " · \(FF.dur(d))" }
    return s
  }
}

/// One-tap validation card: the predicted type first, other types correct it, "Pas une séance".
struct FormeReviewCard: View {
  let session: SportSession
  @ObservedObject var sport: SportController
  var body: some View {
    let k = SportKind.of(session.predicted)
    FrostCard(radius: 32, padding: 16, spacing: 12) {
      NavigationLink(value: FormeRoute.session(session.id)) {
        HStack(spacing: 12) {
          Image(systemName: k.icon).font(.system(size: 18, weight: .semibold)).foregroundStyle(Forme.strainInk)
            .frame(width: 44, height: 44).background(Forme.strainBg, in: Circle())
          VStack(alignment: .leading, spacing: 2) {
            Text("\(k.title) ?").font(Forme.sans(16, .bold, relativeTo: .headline)).foregroundStyle(Forme.ink)
            Text("\(FF.day(session.start, "EEE d · HH:mm").capitalized)\(session.durationMin.map { " · \(FF.dur($0))" } ?? "")")
              .font(Forme.sans(12, .medium, relativeTo: .caption)).foregroundStyle(Forme.muted)
          }
          .frame(maxWidth: .infinity, alignment: .leading)
          if let c = session.confidence {
            Text("\(Int((c * 100).rounded())) %").font(Forme.sans(13, .semibold, relativeTo: .footnote)).foregroundStyle(c < 0.6 ? Forme.strainInk : Forme.muted)
          }
          Image(systemName: "chevron.right").font(.system(size: 13, weight: .semibold)).foregroundStyle(Forme.muted)
        }
      }
      .buttonStyle(.plain)
      ScrollView(.horizontal, showsIndicators: false) {
        HStack(spacing: 8) {
          chip("Oui, \(k.title.lowercased())", filled: true) { sport.annotate("session", session.id, "confirm") }
          ForEach(SportKind.allCases.filter { $0 != k }) { o in chip(o.title) { sport.annotate("session", session.id, "type", o.rawValue) } }
          chip("Pas une séance") { sport.annotate("session", session.id, "reject") }
        }
      }
      .scrollClipDisabled()
    }
    .contextMenu { Button("Supprimer la séance", systemImage: "trash", role: .destructive) { sport.delete(session.id) } }
  }

  private func chip(_ t: String, filled: Bool = false, _ a: @escaping () -> Void) -> some View {
    Button(action: a) {
      Text(t).font(Forme.sans(13, .semibold, relativeTo: .footnote))
        .foregroundStyle(filled ? Forme.canvas : Forme.ink)
        .padding(.horizontal, 14).frame(minHeight: 36)
        .background(filled ? AnyShapeStyle(Forme.ink) : AnyShapeStyle(Forme.sunken), in: Capsule())
    }
    .buttonStyle(.plain)
  }
}

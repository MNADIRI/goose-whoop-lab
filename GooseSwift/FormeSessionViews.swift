import SwiftUI

// MARK: - Session detail

/// Level 3 · one session (course, musculation, tennis, autre): effort, pulse with sets / exchanges
/// shaded, zones, exercises and repetitions (strength), detection with one-tap confirmation.
struct FormeSessionView: View {
  let sessionID: String
  @ObservedObject var sport: SportController
  @Environment(\.dismiss) private var dismiss
  @State private var newExerciseFor: [SportSegment]?
  @State private var newExercise = ""
  @State private var confirmDelete = false

  private var session: SportSession? { sport.sessions.first { $0.id == sessionID } }

  var body: some View {
    FormeScroll {
      DetailHeader(title: "Séance", chip: session.map { FF.day($0.start, "EEE d").capitalized })
      if let s = session {
        let kind = SportKind.of(s.type)
        hero(s, kind).padding(.top, 24)
        pulse(s, kind)
        if kind == .musculation { exercises(s) }
        if kind == .tennis { tennis(s) }
        if kind != .musculation { zones(s) }
        if kind == .course { running(s) }
        // a session you started yourself has a known type: no "detection" to confirm
        if !s.source.contains("manual") { detection(s, kind) }
        TechDetails(subtitle: "Modèle, capteurs et comptage", rows: tech(s)).padding(.top, 32)
        Button(role: .destructive) { confirmDelete = true } label: {
          Label("Supprimer la séance", systemImage: "trash").font(Forme.sans(15, .semibold, relativeTo: .subheadline))
            .frame(maxWidth: .infinity, minHeight: 52)
            .foregroundStyle(Forme.strainInk)
            .background(Forme.strainBg, in: Capsule())
        }
        .buttonStyle(.plain)
        .padding(.top, 24)
      } else {
        FrostCard { Text("Séance introuvable").font(Forme.sans(17, .semibold)).foregroundStyle(Forme.ink) }.padding(.top, 24)
      }
    }
    .formeScreen()
    .onAppear { sport.refresh() }
    .confirmationDialog("Supprimer cette séance ?", isPresented: $confirmDelete, titleVisibility: .visible) {
      Button("Supprimer", role: .destructive) { sport.delete(sessionID); dismiss() }
      Button("Annuler", role: .cancel) {}
    } message: { Text("Elle disparaît de l'historique et du jeu de données. Cette action est définitive.") }
    .alert("Nouvel exercice", isPresented: Binding(get: { newExerciseFor != nil }, set: { if !$0 { newExerciseFor = nil } })) {
      TextField("Nom", text: $newExercise)
      Button("Enregistrer") {
        let name = newExercise.trimmingCharacters(in: .whitespaces)
        if !name.isEmpty { for seg in newExerciseFor ?? [] { sport.annotate("segment", seg.id, "exercise", name) } }
        newExercise = ""; newExerciseFor = nil
      }
      Button("Annuler", role: .cancel) { newExerciseFor = nil }
    }
  }

  // MARK: hero

  private func strain(_ s: SportSession) -> Double? {
    guard s.hrSeries.count >= 3, let rest = s.restHR, let mx = s.maxHR, mx > rest else { return nil }
    var trimp = 0.0
    for p in s.hrSeries {
      let pct = p.1 / mx
      let w: Double = pct >= 0.9 ? 5 : (pct >= 0.8 ? 4 : (pct >= 0.7 ? 3 : (pct >= 0.6 ? 2 : (pct >= 0.5 ? 1 : 0))))
      trimp += w
    }
    return 21 * (1 - exp(-trimp / 250))
  }

  private func statusWord(_ s: SportSession) -> String {
    if s.isValidated { return s.source.contains("manual") ? "Lancée" : "Validée" }
    return s.labelStatus == "pending" ? "À vérifier" : "Détectée"
  }

  private func hero(_ s: SportSession, _ kind: SportKind) -> some View {
    let st = strain(s)
    return AuraCard(family: .effort, radius: 44, padding: 24, height: 300) {
      HStack {
        Text(kind == .course ? "Course à pied" : kind.title).font(Forme.sans(20, .regular, relativeTo: .title3)).tracking(-0.2)
        Spacer()
        VeilPill(text: statusWord(s), ink: Forme.Family.effort.pillInk)
      }
      DotNumber(value: st.map { FF.num($0, 1) }, size: 80, unit: st == nil ? nil : "/ 21", unitSize: 22).padding(.top, 20)
      Text("\(FF.clock(s.start)) – \(FF.clock(s.end)) · \(FF.dur(s.durationMin))").font(Forme.sans(15, .regular, relativeTo: .body)).foregroundStyle(Forme.hex(0x1c2426)).padding(.top, 12)
      Spacer(minLength: 12)
      VeilPanel {
        HStack(spacing: 8) {
          switch kind {
          case .course:
            MiniStat(label: "Distance", value: s.gpsDistanceM.map { FF.num($0 / 1000, 1) } ?? "—", unit: "km", labelColor: Forme.onBright2)
            MiniStat(label: "Allure", value: pace(s.gpsPaceSPerKm), unit: "/km", labelColor: Forme.onBright2)
            MiniStat(label: "Dénivelé", value: s.gpsGainM.map { "+\(FF.num($0))" } ?? "—", unit: "m", labelColor: Forme.onBright2)
          case .musculation:
            let g = s.exerciseGroups
            let sets = g.reduce(0) { $0 + $1.sets.count }
            MiniStat(label: "Séries", value: "\(sets)", labelColor: Forme.onBright2)
            MiniStat(label: "Répétitions", value: "\(g.reduce(0) { $0 + $1.sets.reduce(0) { $0 + ($1.reps ?? 0) } })", labelColor: Forme.onBright2)
            MiniStat(label: "Repos médian", value: medianRest(s).map { FF.num($0) } ?? "—", unit: "s", labelColor: Forme.onBright2)
          case .tennis:
            MiniStat(label: "Frappes", value: "\(s.strokes ?? 0)", labelColor: Forme.onBright2)
            MiniStat(label: "Échanges", value: "\(s.segments.filter { $0.kind == "exchange" }.count)", labelColor: Forme.onBright2)
            MiniStat(label: "Pouls moyen", value: FF.num(s.hrMean), unit: "bpm", labelColor: Forme.onBright2)
          case .autre:
            MiniStat(label: "Pouls moyen", value: FF.num(s.hrMean), unit: "bpm", labelColor: Forme.onBright2)
            MiniStat(label: "Pouls max", value: FF.num(s.hrMax), unit: "bpm", labelColor: Forme.onBright2)
            MiniStat(label: "Durée", value: FF.dur(s.durationMin), labelColor: Forme.onBright2)
          }
        }
      }
    }
  }

  private func pace(_ v: Double?) -> String {
    guard let v, v.isFinite, v > 0 else { return "—" }
    return "\(Int(v) / 60)′\(String(format: "%02d", Int(v) % 60))″"
  }

  private func medianRest(_ s: SportSession) -> Double? {
    let sets = s.segments.filter { $0.kind == "set" && $0.labelStatus != "rejected" }.sorted { $0.start < $1.start }
    let r = zip(sets, sets.dropFirst()).map { $1.start.timeIntervalSince($0.end) }.sorted()
    return r.isEmpty ? nil : r[r.count / 2]
  }

  // MARK: pulse

  @ViewBuilder private func pulse(_ s: SportSession, _ kind: SportKind) -> some View {
    SectionTitle(title: "Pouls", subtitle: kind == .musculation ? "Les séries sont grisées" : (kind == .tennis ? "Les échanges sont grisés" : "Pendant la séance"))
    FrostCard(radius: 36, padding: 20, spacing: 12) {
      if s.hrSeries.count >= 2 {
        HRChart(points: s.hrSeries, bands: s.segments.filter { $0.kind == "set" || $0.kind == "exchange" }.map { ($0.start, $0.end) })
          .frame(height: 130)
          .accessibilityLabel("Pouls de la séance, moyenne \(FF.num(s.hrMean)), maximum \(FF.num(s.hrMax))")
        HStack {
          Text(FF.clock(s.hrSeries.first?.0)); Spacer(); Text(FF.clock(s.hrSeries.last?.0))
        }
        .font(Forme.sans(12, .medium, relativeTo: .caption)).foregroundStyle(Forme.muted)
      } else {
        Text("Pas encore de pouls pour cette séance.").font(Forme.sans(15, .regular)).foregroundStyle(Forme.ink2)
      }
      HStack(spacing: 8) {
        MiniStat(label: "Moyen", value: FF.num(s.hrMean), unit: "bpm")
        MiniStat(label: "Max", value: FF.num(s.hrMax), unit: "bpm")
        MiniStat(label: "Capteurs 100 Hz", value: FF.num(s.imuMinutes), unit: "min")
      }
    }
    .padding(.top, 12)
  }

  // MARK: zones

  @ViewBuilder private func zones(_ s: SportSession) -> some View {
    if let mx = s.maxHR, s.hrSeries.count >= 2 {
      let bounds = [0.5, 0.6, 0.7, 0.8, 0.9, 10]
      let mins = (0..<5).map { z in Double(s.hrSeries.filter { $0.1 / mx >= bounds[z] && $0.1 / mx < bounds[z + 1] }.count) }
      let top = max(mins.max() ?? 1, 1)
      let colors = [Forme.hex(0xcfe0f7), Forme.hex(0x9cc3f5), Forme.hex(0xf3a6ae), Forme.hex(0xe0606b), Forme.strainDeep]
      SectionTitle(title: "Zones cardiaques", subtitle: "En % de ta fréquence cardiaque max (\(FF.num(mx)) bpm)")
      FrostCard(radius: 36, padding: 20, spacing: 12) {
        ForEach(0..<5, id: \.self) { i in
          HStack(spacing: 12) {
            Text("Zone \(i + 1)").font(Forme.sans(13, .semibold, relativeTo: .footnote)).foregroundStyle(Forme.ink).frame(width: 60, alignment: .leading)
            Track(fraction: mins[i] / top, fill: colors[i], height: 10)
            Text(FF.dur(mins[i])).font(Forme.sans(13, .semibold, relativeTo: .footnote)).monospacedDigit().foregroundStyle(Forme.ink).frame(width: 60, alignment: .trailing)
          }
          .accessibilityElement(children: .combine)
        }
      }
      .padding(.top, 12)
    }
  }

  @ViewBuilder private func running(_ s: SportSession) -> some View {
    LazyVGrid(columns: [GridItem(.flexible(), spacing: 12), GridItem(.flexible(), spacing: 12)], spacing: 12) {
      MetricTile(label: "Cadence", value: FF.num(s.cadence), unit: "pas/min", note: "Médiane de la séance")
      MetricTile(label: "Durée", value: FF.dur(s.durationMin), note: s.gpsDistanceM == nil ? "Sans GPS" : "GPS du téléphone")
    }
    .padding(.top, 12)
  }

  @ViewBuilder private func tennis(_ s: SportSession) -> some View {
    let ex = s.segments.filter { $0.kind == "exchange" }
    SectionTitle(title: "Frappes", subtitle: "Gestes isolés détectés au poignet")
    HStack(spacing: 12) {
      MetricTile(label: "Frappes", value: "\(s.strokes ?? 0)", note: "Sur toute la séance")
      MetricTile(label: "Par échange", value: ex.isEmpty ? "—" : FF.num(Double(ex.compactMap(\.repsPred).reduce(0, +)) / Double(ex.count), 1), note: "\(ex.count) échanges")
    }
    .padding(.top, 12)
  }

  // MARK: exercises (strength)

  @ViewBuilder private func exercises(_ s: SportSession) -> some View {
    let groups = s.exerciseGroups
    SectionTitle(title: "Exercices", subtitle: "Un nom suivi de « ? » attend ta confirmation")
    if groups.isEmpty {
      FrostCard { Text((s.imuMinutes ?? 0) > 0 ? "Aucune série répétitive trouvée." : "Pas de capteurs 100 Hz sur cette séance.").font(Forme.sans(15, .regular)).foregroundStyle(Forme.ink2) }.padding(.top, 12)
    }
    ForEach(Array(groups.enumerated()), id: \.element.key) { gi, g in
      let guessed = g.sets.allSatisfy { $0.exerciseLabel == nil }
      FrostCard(radius: 36, padding: 20, spacing: 12) {
        HStack(alignment: .center, spacing: 8) {
          Menu {
            if let name = g.name, guessed {
              Button("Oui, \(name)", systemImage: "checkmark") { for set in g.sets { sport.annotate("segment", set.id, "exercise", name) } }
              Divider()
            }
            ForEach(sport.exercises.filter { $0 != g.name }, id: \.self) { e in
              Button(e) { for set in g.sets { sport.annotate("segment", set.id, "exercise", e) } }
            }
            Button("Nouvel exercice…") { newExerciseFor = g.sets }
          } label: {
            HStack(spacing: 4) {
              Text(g.name.map { guessed ? "\($0) ?" : $0 } ?? "Exercice \(gi + 1) ?").font(Forme.sans(18, .semibold, relativeTo: .headline))
              Image(systemName: "chevron.up.chevron.down").font(.system(size: 11, weight: .semibold))
            }
            .foregroundStyle(guessed ? Forme.strainInk : Forme.ink)
          }
          Spacer()
          if guessed, let name = g.name {
            Button { for set in g.sets { sport.annotate("segment", set.id, "exercise", name) } } label: {
              Text("Valider").font(Forme.sans(13, .semibold)).foregroundStyle(Forme.canvas).padding(.horizontal, 14).frame(minHeight: 36).background(Forme.ink, in: Capsule())
            }
            .buttonStyle(.plain)
          } else if !guessed {
            FormePill(text: "Validé")
          }
        }
        Text("\(g.sets.count) × " + g.sets.map { "\($0.reps ?? 0)" }.joined(separator: " / ") + " rép.")
          .font(Forme.sans(13, .semibold, relativeTo: .footnote)).monospacedDigit().foregroundStyle(Forme.muted)
        VStack(spacing: 0) {
          ForEach(Array(g.sets.enumerated()), id: \.element.id) { i, seg in
            if i > 0 { FormeDivider() }
            FormeSetRow(index: i + 1, seg: seg, sport: sport)
          }
        }
      }
      .padding(.top, 12)
    }
  }

  // MARK: detection

  @ViewBuilder private func detection(_ s: SportSession, _ kind: SportKind) -> some View {
    SectionTitle(title: "Détection", subtitle: "Type reconnu par l'app, à confirmer d'un geste")
    FrostCard(radius: 36, padding: 20, spacing: 12) {
      ForEach(SportKind.allCases) { k in
        HStack(spacing: 12) {
          Text(k.title).font(Forme.sans(14, .semibold, relativeTo: .subheadline)).foregroundStyle(Forme.ink).frame(width: 100, alignment: .leading)
          Track(fraction: s.probs[k.rawValue] ?? 0, fill: k == kind ? Forme.strain : Forme.dotOff, height: 8)
          Text("\(Int(((s.probs[k.rawValue] ?? 0) * 100).rounded())) %").font(Forme.sans(12, .semibold, relativeTo: .caption)).monospacedDigit().foregroundStyle(Forme.muted).frame(width: 40, alignment: .trailing)
        }
        .accessibilityElement(children: .combine)
      }
      HStack(spacing: 10) {
        Button { sport.annotate("session", s.id, "confirm") } label: {
          Text(s.isValidated ? "Validée" : "Oui, \(kind.title.lowercased())").font(Forme.sans(15, .semibold))
            .foregroundStyle(Forme.canvas).frame(maxWidth: .infinity, minHeight: 48).background(s.isValidated ? Forme.green : Forme.ink, in: Capsule())
        }
        .buttonStyle(.plain)
        .disabled(s.isValidated)
        Menu {
          ForEach(SportKind.allCases.filter { $0 != kind }) { k in Button(k.title) { sport.annotate("session", s.id, "type", k.rawValue) } }
          Button("Pas une séance") { sport.annotate("session", s.id, "reject") }
        } label: {
          Text("Corriger le type").font(Forme.sans(15, .semibold)).foregroundStyle(Forme.ink).frame(maxWidth: .infinity, minHeight: 48).background(Forme.sunken, in: Capsule())
        }
      }
      .padding(.top, 4)
    }
    .padding(.top, 12)
  }

  private func tech(_ s: SportSession) -> [(String, String, String)] {
    [
      ("Modèle", "Version qui a classé la séance", s.modelVersion ?? "—"),
      ("Confiance", "Probabilité du type retenu", s.confidence.map { "\(Int(($0 * 100).rounded())) %" } ?? "—"),
      ("Origine", "Détection automatique ou départ manuel", s.source.contains("manual") ? "Manuelle" : "Automatique"),
      ("Capteurs 100 Hz", "Accéléromètre et gyroscope enregistrés", s.imuMinutes.map { "\(FF.num($0, 1)) min" } ?? "—"),
      ("Pouls de repos · max", "Référence des zones", "\(FF.num(s.restHR)) · \(FF.num(s.maxHR)) bpm"),
      ("Séries détectées", "Mouvements répétitifs nommés", "\(s.sets ?? 0)"),
    ]
  }
}

/// One strength set: reps − / +, validation, concentric / eccentric / pause.
struct FormeSetRow: View {
  let index: Int
  let seg: SportSegment
  @ObservedObject var sport: SportController
  var body: some View {
    VStack(alignment: .leading, spacing: 8) {
      HStack(spacing: 12) {
        VStack(alignment: .leading, spacing: 1) {
          Text("Série \(index)").font(Forme.sans(14, .semibold, relativeTo: .subheadline)).foregroundStyle(Forme.ink)
          Text("\(FF.clock(seg.start)) · \(Int(seg.end.timeIntervalSince(seg.start))) s").font(Forme.sans(12, .medium, relativeTo: .caption)).foregroundStyle(Forme.muted)
        }
        Spacer()
        Button { sport.annotate("segment", seg.id, "reps", "\(max(0, (seg.reps ?? 0) - 1))") } label: {
          Image(systemName: "minus").font(.system(size: 14, weight: .bold)).frame(width: 36, height: 36).background(Forme.sunken, in: Circle())
        }
        .buttonStyle(.plain).accessibilityLabel("Une répétition de moins")
        Text("\(seg.reps ?? 0)").font(Forme.sans(22, .semibold, relativeTo: .title3)).monospacedDigit().frame(minWidth: 34)
        Button { sport.annotate("segment", seg.id, "reps", "\((seg.reps ?? 0) + 1)") } label: {
          Image(systemName: "plus").font(.system(size: 14, weight: .bold)).frame(width: 36, height: 36).background(Forme.sunken, in: Circle())
        }
        .buttonStyle(.plain).accessibilityLabel("Une répétition de plus")
        Button { sport.annotate("segment", seg.id, seg.isValidated ? "reject" : "confirm") } label: {
          Image(systemName: seg.isValidated ? "checkmark.circle.fill" : "checkmark.circle").font(.system(size: 26, weight: .regular))
            .foregroundStyle(seg.isValidated ? Forme.green : Forme.muted).frame(width: 44, height: 44)
        }
        .buttonStyle(.plain)
        .accessibilityLabel(seg.isValidated ? "Série validée, toucher pour l'écarter" : "Valider la série")
      }
      .foregroundStyle(Forme.ink)
      if let up = seg.upS, let down = seg.downS {
        let concUp = seg.concentric == "up"
        let c = concUp ? up : down, e = concUp ? down : up, h = seg.holdS ?? 0
        HStack(spacing: 8) {
          SegmentBar(parts: [(c, Forme.strain), (e, Forme.hex(0xf3a6ae)), (max(h, 0.05), Forme.dotOff)], height: 6)
          Menu {
            Button("Concentrique en montée") { sport.annotate("segment", seg.id, "concentric", "up") }
            Button("Concentrique en descente") { sport.annotate("segment", seg.id, "concentric", "down") }
          } label: {
            Text("\(FF.num(c, 1)) · \(FF.num(e, 1)) · \(FF.num(h, 1)) s").font(Forme.sans(12, .medium, relativeTo: .caption)).monospacedDigit().foregroundStyle(Forme.muted)
          }
        }
        .accessibilityLabel("Concentrique \(FF.num(c, 1)) s, excentrique \(FF.num(e, 1)) s, pause \(FF.num(h, 1)) s")
      }
    }
    .padding(.vertical, 12)
  }
}

/// Pulse line with shaded bands (sets / exchanges).
struct HRChart: View {
  let points: [(Date, Double)]
  var bands: [(Date, Date)] = []
  var body: some View {
    GeometryReader { g in
      let w = g.size.width, h = g.size.height
      let t0 = points.first?.0.timeIntervalSince1970 ?? 0
      let t1 = max(points.last?.0.timeIntervalSince1970 ?? 1, t0 + 1)
      let lo = (points.map(\.1).min() ?? 50) - 5, hi = (points.map(\.1).max() ?? 150) + 5
      let x: (Date) -> CGFloat = { CGFloat(($0.timeIntervalSince1970 - t0) / (t1 - t0)) * w }
      let y: (Double) -> CGFloat = { h - CGFloat(($0 - lo) / max(hi - lo, 1)) * h }
      ZStack(alignment: .topLeading) {
        ForEach(Array(bands.enumerated()), id: \.offset) { _, b in
          RoundedRectangle(cornerRadius: 6).fill(Forme.sunken).frame(width: max(3, x(b.1) - x(b.0)), height: h).offset(x: x(b.0))
        }
        Path { p in
          for (i, pt) in points.enumerated() {
            let c = CGPoint(x: x(pt.0), y: y(pt.1))
            if i == 0 { p.move(to: c) } else { p.addLine(to: c) }
          }
        }
        .stroke(Forme.strain, style: StrokeStyle(lineWidth: 2.2, lineCap: .round, lineJoin: .round))
        if let peak = points.max(by: { $0.1 < $1.1 }) {
          Circle().fill(Forme.strain).frame(width: 9, height: 9).overlay(Circle().stroke(.white, lineWidth: 2)).position(x: x(peak.0), y: y(peak.1))
        }
      }
    }
  }
}

// MARK: - Start an activity

/// The red button's sheet: one tap starts a sport session (the same engine as automatic detection,
/// so it lands in Effort › Séances). Running also starts the phone's GPS.
struct FormeStartSheet: View {
  @ObservedObject var sport: SportController
  @ObservedObject var tracker: ActivityLocationTracker
  let onStarted: () -> Void

  var body: some View {
    VStack(alignment: .leading, spacing: 16) {
      Text("Démarrer une activité").font(Forme.sans(24, .regular, relativeTo: .title2)).foregroundStyle(Forme.ink)
      Text("Enregistrée dans Effort › Séances, capteurs du bracelet à 100 Hz.").font(Forme.sans(13, .medium, relativeTo: .footnote)).foregroundStyle(Forme.muted)
        .fixedSize(horizontal: false, vertical: true)
      LazyVGrid(columns: [GridItem(.flexible(), spacing: 12), GridItem(.flexible(), spacing: 12)], spacing: 12) {
        ForEach(SportKind.allCases) { k in
          Button {
            sport.startManual(k)
            if k == .course { tracker.start(reset: true) }
            UIImpactFeedbackGenerator(style: .medium).impactOccurred()
            onStarted()
          } label: {
            VStack(alignment: .leading, spacing: 10) {
              Image(systemName: k.icon).font(.system(size: 24, weight: .semibold)).foregroundStyle(Forme.strainInk)
                .frame(width: 48, height: 48).background(Forme.strainBg, in: Circle())
              Text(k == .course ? "Course à pied" : k.title).font(Forme.sans(16, .semibold, relativeTo: .headline)).foregroundStyle(Forme.ink)
            }
            .padding(16)
            .frame(maxWidth: .infinity, minHeight: 112, alignment: .leading)
            .background(Forme.inset, in: RoundedRectangle(cornerRadius: 28, style: .continuous))
          }
          .buttonStyle(FormePressStyle())
        }
      }
      Toggle(isOn: $sport.enabled) {
        VStack(alignment: .leading, spacing: 1) {
          Text("Détection automatique").font(Forme.sans(15, .semibold)).foregroundStyle(Forme.ink)
          Text("Course, tennis, musculation reconnus seuls").font(Forme.sans(12, .medium)).foregroundStyle(Forme.muted)
        }
      }
      .tint(Forme.ink)
      Spacer(minLength: 0)
    }
    .padding(24)
    .background(FormeGround())
  }
}

// MARK: - Live session

struct FormeLiveSessionView: View {
  @ObservedObject var sport: SportController
  @ObservedObject var ble: GooseBLEClient
  @ObservedObject var tracker: ActivityLocationTracker
  @Environment(\.dismiss) private var dismiss
  @State private var confirmCancel = false

  private var kind: SportKind { SportKind.of(sport.current["label_type"] as? String ?? sport.current["guess"] as? String) }
  private var start: Date { (sport.current["start_ts"] as? Int).map { Date(timeIntervalSince1970: TimeInterval($0)) } ?? Date() }
  private var running: Bool { sport.state == "active" || sport.state == "ending" }

  var body: some View {
    VStack(spacing: 0) {
      HStack {
        Text(running ? (sport.current["label_type"] == nil ? "\(kind.title) probable" : (kind == .course ? "Course à pied" : kind.title)) : "Séance terminée")
          .font(Forme.sans(20, .regular, relativeTo: .title3)).foregroundStyle(Forme.ink)
        Spacer()
        FormePill(text: sport.capturing ? "Capteurs 100 Hz" : "Historique 1 Hz", ink: Forme.strainInk, bg: Forme.strainBg)
      }
      .padding(.top, 28)
      TimelineView(.periodic(from: .now, by: 1)) { ctx in
        Text(running ? elapsed(ctx.date) : "—").font(Forme.dot(84)).monospacedDigit().foregroundStyle(Forme.ink)
          .lineLimit(1).minimumScaleFactor(0.5)
          .frame(maxWidth: .infinity)
          .padding(.top, 28)
          .accessibilityLabel("Durée \(elapsed(ctx.date))")
      }
      LazyVGrid(columns: [GridItem(.flexible(), spacing: 12), GridItem(.flexible(), spacing: 12)], spacing: 12) {
        MetricTile(label: "Pouls", value: ble.liveHeartRateBPM.map { "\($0)" } ?? "—", unit: "bpm", note: "En direct")
        switch kind {
        case .course:
          MetricTile(label: "Distance", value: FF.num(tracker.distanceMeters / 1000, 2), unit: "km", note: tracker.gpsStatus)
        case .musculation:
          MetricTile(label: "Séries · rép.", value: "\(sport.current["sets_last10"] as? Int ?? 0) · \(sport.current["reps_last10"] as? Int ?? 0)", note: "10 dernières minutes")
        case .tennis:
          MetricTile(label: "Frappes", value: "\(sport.current["strokes_last10"] as? Int ?? 0)", note: "10 dernières minutes")
        case .autre:
          MetricTile(label: "Type probable", value: SportKind.of(sport.current["guess"] as? String).title, note: (sport.current["guess_p"] as? Double).map { "\(Int($0 * 100)) %" })
        }
      }
      .padding(.top, 28)
      if kind == .course {
        HStack(spacing: 12) {
          MetricTile(label: "Allure", value: tracker.currentPaceSecondsPerKilometer.map { "\(Int($0) / 60)′\(String(format: "%02d", Int($0) % 60))″" } ?? "—", unit: "/km")
          MetricTile(label: "Dénivelé", value: "+\(FF.num(tracker.elevationGainMeters))", unit: "m")
        }
        .padding(.top, 12)
      }
      Spacer(minLength: 24)
      if running {
        Button {
          let gps: [String: Double]? = kind == .course && tracker.distanceMeters > 0 ? [
            "distance_m": tracker.distanceMeters,
            "pace_s_per_km": tracker.distanceMeters > 0 ? Date().timeIntervalSince(start) / (tracker.distanceMeters / 1000) : 0,
            "elevation_gain_m": tracker.elevationGainMeters,
          ] : nil
          tracker.stop()
          sport.stopManual(gps: gps)
          UINotificationFeedbackGenerator().notificationOccurred(.success)
          dismiss()
        } label: {
          Label("Terminer la séance", systemImage: "stop.fill").font(Forme.sans(17, .semibold))
            .foregroundStyle(.white).frame(maxWidth: .infinity, minHeight: 56).background(Forme.strain, in: Capsule())
        }
        .buttonStyle(.plain)
        Button("Annuler la séance") { confirmCancel = true }
          .font(Forme.sans(15, .semibold)).foregroundStyle(Forme.muted).frame(minHeight: 44).padding(.top, 6)
      } else {
        Button("Fermer") { dismiss() }.font(Forme.sans(17, .semibold)).foregroundStyle(Forme.canvas).frame(maxWidth: .infinity, minHeight: 56).background(Forme.ink, in: Capsule()).buttonStyle(.plain)
      }
    }
    .padding(.horizontal, 24)
    .padding(.bottom, 16)
    .background(FormeGround())
    .confirmationDialog("Annuler cette séance ?", isPresented: $confirmCancel, titleVisibility: .visible) {
      Button("Supprimer la séance", role: .destructive) {
        if let id = sport.current["id"] as? String { sport.delete(id) }
        tracker.stop()
        dismiss()
      }
      Button("Continuer la séance", role: .cancel) {}
    } message: { Text("Elle ne sera pas enregistrée.") }
  }

  private func elapsed(_ now: Date) -> String {
    let s = max(0, Int(now.timeIntervalSince(start)))
    return s >= 3600 ? String(format: "%d:%02d:%02d", s / 3600, s / 60 % 60, s % 60) : String(format: "%02d:%02d", s / 60, s % 60)
  }
}

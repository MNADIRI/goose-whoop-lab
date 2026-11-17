import SwiftUI

/// Level 3 · Indicators: health signals vs the last 30 nights, fatigue, VO₂max, circadian rhythm,
/// training form, the strap and its data, and the technical pages (trends, methods, diagnostics).
struct FormeIndicatorsView: View {
  @EnvironmentObject private var model: GooseAppModel
  @EnvironmentObject private var router: AppRouter
  @ObservedObject var labResults: WhoopLabResultsStore
  @ObservedObject private var bodyComp = BodyCompStore.shared
  @ObservedObject private var weekly = WeeklySummaryStore.shared
  let isRoot: Bool

  var body: some View {
    FormeScroll {
      let day = FormeDay(raw: labResults.raw, key: FormeDay.latestNightKey(labResults.raw))
      DetailHeader(title: "Indicateurs", chip: day.map(nightChip) ?? "Aujourd'hui", showsBack: !isRoot)
      if let day {
        signals(day).padding(.top, 24)
        HStack(spacing: 12) {
          MetricTile(label: "Fatigue", value: FF.num(day.fatigue), unit: "/100", note: day.fatigue.map { $0 < 40 ? "Basse · pas de surentraînement" : ($0 < 65 ? "Modérée" : "Élevée") } ?? day.fatiguePending, noteColor: (day.fatigue ?? 0) < 40 && day.fatigue != nil ? Forme.posInk : Forme.muted)
          MetricTile(label: "VO₂max", value: day.vo2max.map { "≈ \(FF.num($0))" } ?? "—", unit: "ml/kg/min")
        }
        .padding(.top, 12)
        circadian(day)
        form(day)
      }
      FormeWeeklyCard(store: weekly)
      FormeBodyCompCard(store: bodyComp)
      device(day)
      technical
    }
    .refreshable { labResults.refresh(force: true); bodyComp.refresh(); weekly.refresh() }
    .formeScreen()
    .onAppear {
      bodyComp.refresh()
      // a new measurement changes the resting energy of the days after it
      bodyComp.onMeasurementsChanged = { [weak labResults] in labResults?.refresh(force: true) }
    }
  }

  private func signals(_ day: FormeDay) -> some View {
    let alert = day.warningLevel == "alert" || day.warningLevel == "watch"
    return FrostCard(radius: 44, padding: 24, spacing: 16) {
      HStack(spacing: 14) {
        FormeDisc(systemImage: alert ? "exclamationmark.shield" : "checkmark.shield", fg: alert ? Forme.strainInk : Forme.posInk, bg: alert ? Forme.strainBg : Forme.posBg, size: 52, shadow: false)
        VStack(alignment: .leading, spacing: 2) {
          Text("Signaux de santé").font(Forme.sans(13, .semibold, relativeTo: .footnote)).foregroundStyle(Forme.muted)
          Text(day.signalsTitle).font(Forme.sans(28, .light, relativeTo: .title)).tracking(-0.56).foregroundStyle(Forme.ink).lineLimit(2).minimumScaleFactor(0.7)
        }
      }
      Text("Chaque signal de la nuit comparé à tes 30 dernières nuits. Deux signaux décalés en même temps déclenchent une alerte.")
        .font(Forme.sans(12, .medium, relativeTo: .caption)).foregroundStyle(Forme.muted)
      VStack(spacing: 0) {
        ForEach(Array(day.signals.enumerated()), id: \.offset) { i, s in
          if i > 0 { FormeDivider() }
          HStack(spacing: 12) {
            Image(systemName: s.flagged ? "exclamationmark" : "checkmark").font(.system(size: 12, weight: .bold))
              .foregroundStyle(s.flagged ? Forme.strainInk : Forme.posInk)
              .frame(width: 28, height: 28).background(s.flagged ? Forme.strainBg : Forme.posBg, in: Circle())
            VStack(alignment: .leading, spacing: 1) {
              Text(s.label).font(Forme.sans(14, .semibold, relativeTo: .subheadline)).foregroundStyle(Forme.ink)
              Text(s.rule).font(Forme.sans(12, .medium, relativeTo: .caption)).foregroundStyle(Forme.muted)
            }
            Spacer()
            Text(s.value).font(Forme.sans(15, .semibold, relativeTo: .subheadline)).monospacedDigit().foregroundStyle(s.flagged ? Forme.strainInk : Forme.ink)
          }
          .padding(.vertical, 12)
          .accessibilityElement(children: .combine)
        }
      }
    }
  }

  @ViewBuilder private func circadian(_ day: FormeDay) -> some View {
    let c = day.root["circadian"]
    let trough = c["heart_rate"]["trough_h"].d
    let skinPeak = c["skin_temp"]["acrophase_h"].d
    SectionTitle(title: "Rythme circadien", subtitle: "Sur les 7 derniers jours")
    FrostCard(radius: 36, padding: 20, spacing: 14) {
      GeometryReader { g in
        let w = g.size.width
        let x: (Double) -> CGFloat = { CGFloat($0 / 24) * w }
        // markers stay inside the track; two markers within the same hour sit side by side
        let near = trough.flatMap { t in skinPeak.map { abs($0 - t) < 1.5 } } ?? false
        let mark: (Double, CGFloat) -> CGFloat = { h, nudge in min(max(0, x(h) - 10 + (near ? nudge : 0)), w - 20) }
        let cal = Calendar.current
        let on = day.onset.map { Date(timeIntervalSince1970: $0) }.map { Double(cal.component(.hour, from: $0)) + Double(cal.component(.minute, from: $0)) / 60 }
        let off = day.wake.map { Date(timeIntervalSince1970: $0) }.map { Double(cal.component(.hour, from: $0)) + Double(cal.component(.minute, from: $0)) / 60 }
        ZStack(alignment: .leading) {
          Capsule().fill(Forme.sunken).frame(height: 12)
          if let on, let off {
            if on > off {
              Capsule().fill(Forme.sleepLight).frame(width: max(6, x(off)), height: 12)
              Capsule().fill(Forme.sleepLight).frame(width: max(8, w - x(on)), height: 12).offset(x: min(x(on), w - 8))
            } else {
              Capsule().fill(Forme.sleepLight).frame(width: max(6, x(off) - x(on)), height: 12).offset(x: x(on))
            }
          }
          if let trough { Circle().fill(Forme.blue).frame(width: 20, height: 20).overlay(Circle().stroke(.white, lineWidth: 3)).offset(x: mark(trough, -9)) }
          if let skinPeak { Circle().fill(Forme.ink).frame(width: 20, height: 20).overlay(Circle().stroke(.white, lineWidth: 3)).offset(x: mark(skinPeak, 9)) }
        }
        .frame(height: 40)
      }
      .frame(height: 40)
      .accessibilityElement(children: .ignore)
      .accessibilityLabel("Sommeil de \(FF.clock(day.onset)) à \(FF.clock(day.wake)), creux du pouls à \(trough.map { "\(Int($0)) h" } ?? "inconnu"), pic de température cutanée à \(skinPeak.map { "\(Int($0)) h" } ?? "inconnu")")
      HStack { Text("0 h"); Spacer(); Text("6 h"); Spacer(); Text("12 h"); Spacer(); Text("18 h"); Spacer(); Text("24 h") }
        .font(Forme.sans(12, .medium, relativeTo: .caption)).foregroundStyle(Forme.muted).padding(.top, -8)
      HStack(alignment: .top, spacing: 8) {
        legendStat("Creux du pouls", trough.map { String(format: "%02d:00", Int($0)) } ?? "—", Forme.blue)
        legendStat("Pic de la peau", skinPeak.map { String(format: "%02d:00", Int($0)) } ?? "—", Forme.ink)
        MiniStat(label: "Régularité", value: FF.num(day.regularity), unit: "/100")
      }
    }
    .padding(.top, 12)
  }

  private func legendStat(_ label: String, _ value: String, _ c: Color) -> some View {
    VStack(alignment: .leading, spacing: 2) {
      HStack(spacing: 6) { Circle().fill(c).frame(width: 8, height: 8); Text(label).lineLimit(1).minimumScaleFactor(0.8) }
        .font(Forme.sans(12, .semibold, relativeTo: .caption)).foregroundStyle(Forme.muted)
      Text(value).font(Forme.sans(20, .semibold, relativeTo: .title3)).monospacedDigit().foregroundStyle(Forme.ink)
    }
    .frame(maxWidth: .infinity, alignment: .leading)
    .accessibilityElement(children: .combine)
  }

  @ViewBuilder private func form(_ day: FormeDay) -> some View {
    let b = day.banister
    SectionTitle(title: "Forme d'entraînement", subtitle: "Modèle de Banister : fatigue récente vs forme de fond")
    FrostCard(radius: 36, padding: 20, spacing: 14) {
      HStack(alignment: .top, spacing: 8) {
        MiniStat(label: "Fatigue récente", value: FF.num(b.atl))
        MiniStat(label: "Forme de fond", value: FF.num(b.ctl))
        MiniStat(label: "Fraîcheur", value: FF.signed(b.tsb))
      }
      if let c = day.formCalibration {
        VStack(alignment: .leading, spacing: 8) {
          HStack(spacing: 8) {
            FormePill(text: "En calibration · \(c.days)/\(c.needed) jours", ink: Forme.ink2, bg: Forme.sunken)
            Spacer(minLength: 0)
          }
          Track(fraction: Double(c.days) / Double(c.needed), fill: Forme.ink2)
          Text("L'équilibre fatigue/forme se lit après 4 semaines de port. D'ici là, les chiffres ci-dessus suivent ta charge sans verdict.")
            .font(Forme.sans(13, .regular, relativeTo: .footnote)).foregroundStyle(Forme.muted)
            .fixedSize(horizontal: false, vertical: true)
        }
      } else {
        formCursor(b.tsb)
      }
    }
    .padding(.top, 12)
  }

  @ViewBuilder private func formCursor(_ tsb: Double?) -> some View {
    VStack(spacing: 14) {
      GeometryReader { g in
        let w = g.size.width
        let t = min(max(((tsb ?? 0) + 30) / 60, 0), 1)
        ZStack(alignment: .leading) {
          HStack(spacing: 3) {
            Capsule().fill(Forme.strainBg)
            Capsule().fill(Forme.posBg)
            Capsule().fill(Forme.calmBg)
          }
          Circle().fill(.white).frame(width: 16, height: 16).overlay(Circle().strokeBorder(Forme.ink, lineWidth: 3)).offset(x: min(max(0, CGFloat(t) * w - 8), w - 16))
        }
      }
      .frame(height: 8)
      .accessibilityLabel("Fraîcheur \(FF.signed(tsb))")
      HStack { Text("En charge"); Spacer(); Text("Équilibré"); Spacer(); Text("Reposé") }
        .font(Forme.sans(12, .medium, relativeTo: .caption)).foregroundStyle(Forme.muted)
    }
  }

  @ViewBuilder private func device(_ day: FormeDay?) -> some View {
    let ble = model.ble
    let connected = ["ready", "connected"].contains(ble.connectionState.lowercased())
    let rows: [(String, String, String, Color)] = [
      ("WHOOP 5.0", connected ? "Connecté\(ble.firmwareVersion.map { " · firmware \($0)" } ?? "")" : "Déconnecté", connected ? "En ligne" : "Hors ligne", connected ? Forme.posInk : Forme.strainInk),
      ("Batterie", "Dernière valeur rapportée", ble.batteryLevelPercent.map { "\($0) %" } ?? "—", Forme.ink),
      ("Pouls en direct", "Flux temps réel", ble.liveHeartRateBPM.map { "\($0) bpm" } ?? "—", Forme.ink),
      ("Dernière synchronisation", "Historique du bracelet", FF.clock(ble.lastHistoricalSyncCompletedAt), Forme.ink),
      ("Port sur 24 h", "Couverture des mesures à la seconde", day?.coverageH.map { "\(FF.num($0, 1)) h" } ?? "—", Forme.ink),
      ("Signal optique accepté", "Fenêtres du capteur de pouls validées", day?.night["optical"]["accepted_window_pct"].d.map { "\(FF.num($0)) %" } ?? "—", Forme.ink),
      ("Artefacts de VFC écartés", "Battements rejetés par le contrôle qualité", day?.night["hrv_qc"]["artifact_pct"].d.map { "\(FF.num($0, 1)) %" } ?? "—", Forme.ink),
      ("Profil", "Âge, poids et taille pour calories et zones", day?.day["age_default_used"].b == true ? "Incomplet" : "Complet", day?.day["age_default_used"].b == true ? Forme.strainInk : Forme.ink),
    ]
    SectionTitle(title: "Bracelet et données", subtitle: "État de la mesure")
    FrostCard(radius: 36, padding: 20, spacing: 0) {
      ForEach(Array(rows.enumerated()), id: \.offset) { i, r in
        if i > 0 { FormeDivider() }
        FormeRow(label: r.0, sub: r.1, value: r.2, tone: r.3)
      }
    }
    .padding(.top, 12)
  }

  private var technical: some View {
    VStack(spacing: 0) {
      SectionTitle(title: "Pour aller plus loin", subtitle: "Historique, méthodes et réglages")
      FrostCard(radius: 36, padding: 8, spacing: 0) {
        link("Respiration et pouls", "Fréquence respiratoire, onde de pouls, SpO₂, capteurs optiques", "waveform.path.ecg", FormeRoute.optical)
        FormeDivider().padding(.horizontal, 12)
        link("Tendances", "Toutes tes nuits et journées", "chart.xyaxis.line", FormeRoute.trends)
        FormeDivider().padding(.horizontal, 12)
        link("Méthodes et sources", "Comment chaque valeur est calculée", "function", FormeRoute.methods)
        FormeDivider().padding(.horizontal, 12)
        Button { router.openMore(nil) } label: { linkLabel("Profil, bracelet et réglages", "Compte, appareil, export, développeur", "gearshape") }.buttonStyle(.plain)
      }
      .padding(.top, 12)
    }
  }

  private func link(_ t: String, _ s: String, _ icon: String, _ r: FormeRoute) -> some View {
    NavigationLink(value: r) { linkLabel(t, s, icon) }.buttonStyle(.plain)
  }

  private func linkLabel(_ t: String, _ s: String, _ icon: String) -> some View {
    HStack(spacing: 12) {
      FormeDisc(systemImage: icon, fg: Forme.ink, bg: Forme.sunken, size: 40, shadow: false)
      VStack(alignment: .leading, spacing: 1) {
        Text(t).font(Forme.sans(15, .semibold, relativeTo: .subheadline)).foregroundStyle(Forme.ink)
        Text(s).font(Forme.sans(12, .medium, relativeTo: .caption)).foregroundStyle(Forme.muted)
      }
      .frame(maxWidth: .infinity, alignment: .leading)
      Image(systemName: "chevron.right").font(.system(size: 13, weight: .semibold)).foregroundStyle(Forme.muted)
    }
    .padding(12)
    .contentShape(Rectangle())
  }
}

// MARK: - Trends

/// Level 3 · Trends: every night and day metric over 7/30/90 days, same visual language as the rest.
struct FormeTrendsView: View {
  @ObservedObject var labResults: WhoopLabResultsStore
  @State private var period = 30

  private struct Metric: Identifiable {
    let id: String
    let title: String
    let unit: String
    let decimals: Int
    let color: Color
    let value: (J) -> Double?
  }

  private static let metrics: [Metric] = [
    Metric(id: "sleep", title: "Sommeil", unit: "h", decimals: 1, color: Forme.blue) { $0["night"]["asleep_min"].d.map { $0 / 60 } },
    Metric(id: "rhr", title: "Pouls au repos", unit: "bpm", decimals: 0, color: Forme.strain) { $0["night"]["resting_hr_bpm"].d },
    Metric(id: "hrv", title: "Variabilité cardiaque (VFC)", unit: "ms", decimals: 0, color: Forme.green) { $0["night"]["hrv_night"]["rmssd_ms"].d },
    Metric(id: "resp", title: "Respiration", unit: "/min", decimals: 1, color: Forme.calmInk) { $0["night"]["respiration"]["rpm"].d },
    Metric(id: "skin", title: "Température cutanée", unit: "°C", decimals: 1, color: Forme.strainInk) { $0["night"]["skin_temp_c"].d },
    Metric(id: "recovery", title: "Récupération", unit: "%", decimals: 0, color: Forme.green) { $0["indices"]["recovery"]["score"].d },
    Metric(id: "strain", title: "Effort", unit: "%", decimals: 0, color: Forme.strain) { $0["day"]["strain_pct"].d ?? $0["day"]["strain_0_21"].d.map { $0 / 21 * 100 } },
    Metric(id: "steps", title: "Pas", unit: "", decimals: 0, color: Forme.ink) { $0["day"]["steps"].d },
    Metric(id: "kcal", title: "Calories actives", unit: "kcal", decimals: 0, color: Forme.strainDeep) { $0["day"]["energy"]["active_kcal"].d },
    Metric(id: "stress", title: "Stress moyen", unit: "/100", decimals: 0, color: Forme.muted) { $0["day"]["stress"]["mean"].d },
  ]

  var body: some View {
    let days = Array(J(labResults.raw)["days"].arr.suffix(period))
    FormeScroll {
      DetailHeader(title: "Tendances")
      HStack {
        Text(rangeLabel(days)).font(Forme.sans(13, .medium, relativeTo: .footnote)).foregroundStyle(Forme.muted)
        Spacer()
        PeriodPicker(period: $period)
      }
      .padding(.top, 20)
      LazyVStack(spacing: 12) {
        ForEach(Self.metrics) { m in card(m, days) }
      }
      .padding(.top, 14)
    }
    .formeScreen()
  }

  private func rangeLabel(_ days: [J]) -> String {
    guard let f = days.first?["date"].s.flatMap(labDate), let l = days.last?["date"].s.flatMap(labDate) else { return "" }
    return "\(FF.day(f, "d MMM")) – \(FF.day(l, "d MMM"))"
  }

  private func card(_ m: Metric, _ days: [J]) -> some View {
    let values = days.compactMap(m.value)
    let avg = values.isEmpty ? nil : values.reduce(0, +) / Double(values.count)
    return FrostCard(radius: 32, padding: 18, spacing: 10) {
      HStack(alignment: .firstTextBaseline) {
        Text(m.title).font(Forme.sans(14, .semibold, relativeTo: .subheadline)).foregroundStyle(Forme.ink2).lineLimit(1)
        Spacer(minLength: 8)
        NumberUnit(value: FF.num(values.last, m.decimals), unit: m.unit, size: 22)
      }
      if values.count > 1 {
        Sparkline(values: values, color: m.color).frame(height: 64)
        Text("Moyenne \(FF.num(avg, m.decimals))\(m.unit.isEmpty ? "" : " \(m.unit)") · \(values.count) jours")
          .font(Forme.sans(12, .medium, relativeTo: .caption)).foregroundStyle(Forme.muted)
      } else {
        Text("Pas encore assez de jours mesurés").font(Forme.sans(12, .medium, relativeTo: .caption)).foregroundStyle(Forme.muted)
      }
    }
    .accessibilityElement(children: .combine)
  }
}

// MARK: - Methods

/// Level 3 · How every value is computed (one source: the strap's raw frames, analysed on the iPhone).
struct FormeMethodsView: View {
  private static let groups: [(String, [(String, String)])] = [
    ("Chaîne de mesure", [
      ("Une seule source", "Bracelet → Bluetooth → trames brutes vérifiées (CRC) → table à la seconde → analyse sur l'iPhone → écrans."),
    ]),
    ("Nuit", [
      ("Sommeil", "État du bracelet + actigraphie Cole-Kripke (te Lindert) au-dessus du bruit du capteur."),
      ("Phases", "Pouls, VFC locale et mouvement, sans EEG : environ 65 à 73 % d'accord au mieux."),
      ("Score de sommeil", "Dimensions RU-SATED (Buysse 2014) : durée vs besoin 40 % (NSF, Hirshkowitz 2015), continuité 30 % (efficacité ≥ 85 %, endormissement ≤ 30 min, éveil ≤ 20 min ; Ohayon 2017), régularité 15 % (milieu de nuit vs 7 nuits ; Phillips 2017, Windred 2024), phases 15 % (profond ≥ 15 %, paradoxal ≥ 20 %)."),
      ("Pouls au repos", "Plus basse moyenne sur 5 min pendant le sommeil."),
      ("VFC", "RMSSD sur battements réellement successifs, battements fendus fusionnés, seuil d'artefact adaptatif."),
      ("Respiration", "Arythmie sinusale respiratoire, pic 0,13–0,45 Hz sur fenêtres de 2 min."),
      ("SpO₂", "Rapport rouge / infrarouge du capteur optique pendant le sommeil immobile."),
      ("Température", "Médiane de la peau pendant le sommeil, écart aux nuits précédentes."),
    ]),
    ("Scores", [
      ("Récupération", "Écarts (z) de tes mesures de la nuit (VFC, pouls au repos…) à ta référence personnelle, pondérés."),
      ("Signaux de santé", "Chaque signal comparé à tes 30 dernières nuits ; deux signaux décalés déclenchent une alerte."),
      ("Fatigue et forme", "Modèle de Banister : charge 7 j (fatigue) contre 42 j (forme de fond), amorcées sur ta première semaine. L'équilibre n'est lu qu'après 28 jours."),
    ]),
    ("Journée", [
      ("Effort", "TRIMP d'Edwards sur zones de FC max (Tanaka), en % de la capacité du jour : 100 × (1 − e^(−TRIMP/250)). 50 % ≈ 1 h soutenue. Charge : 7 j contre 28 j."),
      ("Calories", "Actives : Keytel 2005 au-dessus du repos, moins la part de repos de ces minutes ; totales : + métabolisme de base. Base : Katch-McArdle (370 + 21,6 × masse maigre) avec la dernière mesure de composition prise avant le jour (120 j au plus), sinon Mifflin-St Jeor sur ton profil."),
      ("Stress", "Pouls au-dessus du repos, éveillé et immobile, rapporté à la réserve cardiaque."),
      ("Pas", "Compteur du bracelet : il compte au poignet, le téléphone compte dans la poche."),
      ("Mesures du jour", "Pouls, VFC et respiration sur des fenêtres de 5 min éveillé et au calme (85 % du temps sous le seuil sédentaire du poignet) ; comparées à tes autres journées, pas à la nuit."),
      ("Séances", "Détection sur le pouls et la signature du mouvement, puis type et exercices reconnus par l'IMU. Sans IMU, 7 minutes sur 10 avec un mouvement du poignet ≥ 0,5 g et 30 % de la réserve cardiaque suffisent (sports de raquette, jeux). Un mouvement périodique pendant la marche ou la course (plus de 70 pas/min) ne compte pas comme une série. Au départ automatique, 3 vibrations ; un triple tap sur le bracelet (3 pics brefs à 0,2–0,8 s d'écart, isolés) dans les 10 min annule la séance. Le flux IMU est relancé s'il se tait plus de 20 s."),
      ("Respiration", "La nuit, deux mesures indépendantes : la variabilité du rythme cardiaque (arythmie respiratoire, fenêtres de 2 min) et l'onde de pouls (amplitude, ligne de base et intervalle des battements pendant les salves optiques immobiles, 9 à 30 resp/min, au moins 2 des 3 concordantes). En journée, la seconde méthode sur les salves immobiles. L'onde de Mayer (≈ 6 resp/min, régulation de la pression) est exclue."),
      ("Onde de pouls", "Les salves optiques du sommeil (25 Hz) sont reconstruites à 200 Hz, les battements alignés sur leur montée la plus rapide puis moyennés (médiane) : montée systolique, onde diastolique, indice de réflexion, rapport b/a. Suivi relatif d'une nuit à l'autre ; ce n'est pas une pression artérielle."),
      ("Capteurs optiques", "Audit des trames complètes : 5 créneaux de 2 photodiodes, convertisseur 19 bits (gamme 16/32 µA), compensation du niveau par pas de 10 nA, courant des LED par pas de 10 µA. Photocourant absolu, lumière renvoyée par mA et pulsatilité de chaque canal : LED 1 et 4 vertes (canal de pouls), LED 2 infrarouge probable (contact et SpO₂), LED 3 rouge probable (SpO₂ seulement, jamais transmise), lumière ambiante. Sans paire rouge + infrarouge transmise, pas de saturation continue."),
      ("Semaine", "Samedi → vendredi, comparée à la semaine précédente : séances par type (hors séances écartées), dépense totale et active des jours portés au moins 12 h, pas, sommeil moyen par nuit, récupération, VFC et pouls de repos moyens. Notification le vendredi à 19 h avec les données de la dernière mise à jour."),
      ("Sauna", "Trois thermomètres dans le bracelet : la peau, et deux capteurs internes. Au sauna, le capteur côté extérieur dépasse 38 °C et passe au-dessus de la peau (la chaleur vient de l'air), pendant au moins 5 min, le pouls monte d'au moins 20 bpm au-dessus du repos, et aucune recharge n'a lieu. Entrée et sortie : montée ou chute de plus de 1 °C par minute. Un bain chaud ou un hammam donnent la même signature."),
    ]),
    ("Composition corporelle", [
      ("Mesures", "Export CSV InBody (toutes les colonnes gardées, « - » = absent) ou pesées Santé. Une même pesée importée deux fois, ou vue à la fois en CSV et dans Santé, ne compte qu'une fois."),
      ("Indices", "Masse maigre = poids − masse grasse (≠ muscle squelettique ≠ masse maigre molle). FFMI et FMI = masse / taille² (taille du profil, sinon déduite de l'IMC de l'appareil)."),
      ("Erreur de mesure", "Écart minimal détectable à 95 % entre deux mesures : 1,96 × √2 × erreur type (graisse 0,6 kg, muscle 0,35 kg, masse maigre 0,5 kg, poids 0,5 kg). En dessous, l'écart est affiché « ≈ »."),
      ("Recomposition", "Droite de tendance sur au moins 3 mesures et 28 jours ; une évolution n'est retenue que si elle dépasse sa marge à 95 %. Une variation de masse maigre qui suit surtout l'eau (73 % de la masse maigre) est signalée."),
      ("Asymétrie", "Écart droite/gauche durable : plus de 5 % du même côté sur au moins 3 mesures."),
      ("Contexte bracelet", "Sommeil, VFC, pouls de repos, effort et séances sur les 7, 28, 56 et 84 jours avant chaque mesure, jamais après. Rien n'est interpolé entre deux mesures, et la récupération du jour ne l'utilise pas."),
    ]),
  ]

  var body: some View {
    FormeScroll {
      DetailHeader(title: "Méthodes")
      ForEach(Self.groups, id: \.0) { g in
        SectionTitle(title: g.0)
        FrostCard(radius: 32, padding: 18, spacing: 0) {
          ForEach(Array(g.1.enumerated()), id: \.offset) { i, r in
            if i > 0 { FormeDivider() }
            VStack(alignment: .leading, spacing: 3) {
              Text(r.0).font(Forme.sans(15, .semibold, relativeTo: .subheadline)).foregroundStyle(Forme.ink)
              Text(r.1).font(Forme.sans(13, .medium, relativeTo: .footnote)).foregroundStyle(Forme.muted).fixedSize(horizontal: false, vertical: true)
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.vertical, 12)
          }
        }
        .padding(.top, 12)
      }
    }
    .formeScreen()
  }
}

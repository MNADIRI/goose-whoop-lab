import SwiftUI
import UniformTypeIdentifiers

// MARK: - Overview card (Indicateurs)

/// Latest measurement, its change vs the previous comparable one, link to the detail.
struct FormeBodyCompCard: View {
  @ObservedObject var store: BodyCompStore
  @State private var importing = false

  var body: some View {
    let l = BCVal(store.latest)
    let ch = BCVal(store.summary["change"] as? [String: Any])
    SectionTitle(title: "Composition corporelle", subtitle: store.count > 0 ? "Mesure du \(measuredOn(l)) · \(l.str("device").map { "InBody \($0)" } ?? sourceName(l))" : "Bioimpédance InBody ou Santé")
    if store.count == 0 {
      FrostCard(radius: 36, padding: 20, spacing: 12) {
        Text("Importe l'export CSV de ta balance InBody (ou tes pesées Santé) pour suivre muscle, graisse et eau, et caler ta dépense au repos sur ta masse maigre mesurée.")
          .font(Forme.sans(14, .regular, relativeTo: .subheadline)).foregroundStyle(Forme.ink2)
        BodyCompImportButtons(store: store, importing: $importing)
      }
      .padding(.top, 12)
      .bodyCompImporter(store: store, isPresented: $importing)
    } else {
      NavigationLink(value: FormeRoute.bodyComposition) {
        FrostCard(radius: 36, padding: 20, spacing: 14) {
          LazyVGrid(columns: [GridItem(.flexible(), spacing: 8), GridItem(.flexible(), spacing: 8), GridItem(.flexible(), spacing: 8)], spacing: 14) {
            stat("Poids", l.num("values.weight_kg"), 1, "kg", ch.obj("weight_kg"))
            stat("Graisse", l.num("values.pbf_pct"), 1, "%", ch.obj("pbf_pct"))
            stat("Masse grasse", l.num("derived.fat_mass_kg"), 1, "kg", ch.obj("fat_mass_kg"))
            stat("Muscle squelettique", l.num("values.smm_kg"), 1, "kg", ch.obj("smm_kg"))
            stat("Masse maigre", l.num("derived.ffm_kg"), 1, "kg", ch.obj("ffm_kg"))
            stat("FFMI", l.num("derived.ffmi"), 1, "kg/m²", nil)
          }
          HStack {
            Text(store.count == 1 ? "1 mesure · les évolutions apparaissent dès la 2e" : "\(store.count) mesures · écarts vs la mesure comparable précédente")
              .font(Forme.sans(12, .medium, relativeTo: .caption)).foregroundStyle(Forme.muted)
            Spacer()
            Image(systemName: "chevron.right").font(.system(size: 13, weight: .semibold)).foregroundStyle(Forme.muted)
          }
        }
      }
      .buttonStyle(.plain)
      .padding(.top, 12)
    }
  }

  private func stat(_ label: String, _ v: Double?, _ d: Int, _ unit: String, _ delta: BCVal?) -> some View {
    VStack(alignment: .leading, spacing: 2) {
      Text(label).font(Forme.sans(11, .semibold, relativeTo: .caption2)).foregroundStyle(Forme.muted).lineLimit(1).minimumScaleFactor(0.8)
      NumberUnit(value: FF.num(v, d), unit: unit, size: 22)
      if let delta, let c = delta.num("change") {
        Text(FF.signed(c, d) + (delta["beyond_error"] as? Bool == true ? "" : " ·  ≈"))
          .font(Forme.sans(11, .semibold, relativeTo: .caption2)).monospacedDigit()
          .foregroundStyle(delta["beyond_error"] as? Bool == true ? Forme.ink : Forme.muted)
          .accessibilityLabel(delta["beyond_error"] as? Bool == true ? "Écart \(FF.signed(c, d))" : "Écart \(FF.signed(c, d)), dans l'erreur de mesure")
      }
    }
    .frame(maxWidth: .infinity, alignment: .leading)
  }
}

func measuredOn(_ l: BCVal) -> String {
  guard let ts = l.num("ts") else { return "—" }
  return FF.day(Date(timeIntervalSince1970: ts), "d MMM yyyy")
}

func sourceName(_ l: BCVal) -> String {
  l.str("source") == "healthkit" ? (l.str("device") ?? "Santé") : "CSV"
}

struct BodyCompImportButtons: View {
  @ObservedObject var store: BodyCompStore
  @Binding var importing: Bool
  var body: some View {
    VStack(alignment: .leading, spacing: 8) {
      HStack(spacing: 10) {
        Button { importing = true } label: {
          Label("Importer un CSV", systemImage: "square.and.arrow.down").font(Forme.sans(14, .semibold))
            .foregroundStyle(Forme.canvas).frame(maxWidth: .infinity, minHeight: 46).background(Forme.ink, in: Capsule())
        }
        .buttonStyle(.plain)
        Button { Task { await store.importHealthKit() } } label: {
          Label("Depuis Santé", systemImage: "heart.text.square").font(Forme.sans(14, .semibold))
            .foregroundStyle(Forme.ink).frame(maxWidth: .infinity, minHeight: 46).background(Forme.sunken, in: Capsule())
        }
        .buttonStyle(.plain)
      }
      .disabled(store.busy)
      if let s = store.status { Text(s).font(Forme.sans(12, .medium, relativeTo: .caption)).foregroundStyle(Forme.muted) }
    }
  }
}

extension View {
  func bodyCompImporter(store: BodyCompStore, isPresented: Binding<Bool>) -> some View {
    fileImporter(isPresented: isPresented, allowedContentTypes: [.commaSeparatedText, .plainText, .data], allowsMultipleSelection: true) { result in
      if case .success(let urls) = result { for u in urls { store.importCSV(url: u) } }
    }
  }
}

// MARK: - Detail

/// Level 3 · body composition: composition, muscle, hydration, metabolism, adaptation.
struct FormeBodyCompView: View {
  @ObservedObject var store: BodyCompStore
  @State private var tab = Tab.composition
  @State private var importing = false
  @State private var pendingDelete: String?

  enum Tab: String, CaseIterable, Identifiable {
    case composition = "Composition", muscle = "Muscle", hydration = "Eau", metabolism = "Métabolisme", adaptation = "Adaptation"
    var id: String { rawValue }
  }

  private var s: BCVal { BCVal(store.summary) }
  private var latest: BCVal { BCVal(store.latest) }

  var body: some View {
    FormeScroll {
      DetailHeader(title: "Composition", chip: store.count > 0 ? measuredOn(latest) : nil)
      // full width, five equal tabs, no scrolling (a horizontal scroll clipped the glass capsule)
      HStack(spacing: 0) {
        ForEach(Tab.allCases) { t in
          Button { withAnimation(.snappy(duration: 0.25)) { tab = t } } label: {
            Text(t.rawValue).font(Forme.sans(12, .bold, relativeTo: .footnote)).foregroundStyle(Forme.ink)
              .lineLimit(1).minimumScaleFactor(0.7)
              .frame(maxWidth: .infinity, minHeight: 36).padding(.horizontal, 2)
              .background(tab == t ? Forme.selection : .clear, in: Capsule())
              .contentShape(Capsule())
          }
          .buttonStyle(.plain)
          .accessibilityAddTraits(tab == t ? .isSelected : [])
        }
      }
      .padding(4)
      .glass(Capsule())
      .padding(.top, 20)
      switch tab {
      case .composition: composition
      case .muscle: muscle
      case .hydration: hydration
      case .metabolism: metabolism
      case .adaptation: adaptation
      }
      measurements
    }
    .formeScreen()
    .onAppear { store.refresh() }
    .bodyCompImporter(store: store, isPresented: $importing)
    .confirmationDialog("Supprimer cette mesure ?", isPresented: Binding(get: { pendingDelete != nil }, set: { if !$0 { pendingDelete = nil } }), titleVisibility: .visible) {
      Button("Supprimer", role: .destructive) { if let id = pendingDelete { store.delete(id) }; pendingDelete = nil }
      Button("Annuler", role: .cancel) { pendingDelete = nil }
    }
  }

  // MARK: composition

  @ViewBuilder private var composition: some View {
    let ch = s.obj("change")
    SectionTitle(title: "Recomposition", subtitle: "Muscle et graisse sur toutes tes mesures, au-delà de l'erreur de mesure")
    FrostCard(radius: 36, padding: 20, spacing: 10) {
      let r = s.obj("recomposition")
      Text(recompTitle(r.str("class"))).font(Forme.sans(22, .light, relativeTo: .title2)).foregroundStyle(Forme.ink)
      Text(recompDetail(r)).font(Forme.sans(13, .regular, relativeTo: .footnote)).foregroundStyle(Forme.ink2).fixedSize(horizontal: false, vertical: true)
      if r["hydration_may_explain_lean_change"] as? Bool == true {
        Text("Une partie de la variation de masse maigre suit celle de l'eau : à confirmer à hydratation comparable.").font(Forme.sans(12, .medium, relativeTo: .caption)).foregroundStyle(Forme.strainInk)
      }
    }
    .padding(.top, 12)
    chartCard("Poids", s.series("series.weight_kg"), "kg", 1)
    chartCard("Masse grasse", s.series("series.fat_mass_kg"), "kg", 1)
    chartCard("Muscle squelettique", s.series("series.smm_kg"), "kg", 1)
    if !ch.d.isEmpty {
      SectionTitle(title: "Depuis la mesure précédente", subtitle: "\(FF.num(ch.num("days"))) jours · « ≈ » = dans l'erreur de mesure")
      FrostCard(radius: 36, padding: 20, spacing: 0) {
        deltaRow("Poids", ch.obj("weight_kg"), "kg", 1)
        FormeDivider(); deltaRow("Masse grasse", ch.obj("fat_mass_kg"), "kg", 1)
        FormeDivider(); deltaRow("Graisse", ch.obj("pbf_pct"), "pt", 1)
        FormeDivider(); deltaRow("Muscle squelettique", ch.obj("smm_kg"), "kg", 1)
        FormeDivider(); deltaRow("Masse maigre", ch.obj("ffm_kg"), "kg", 1)
        if let issues = ch.obj("comparability")["issues"] as? [String], !issues.isEmpty {
          Text("Mesures peu comparables : " + issues.joined(separator: " · ")).font(Forme.sans(12, .medium, relativeTo: .caption)).foregroundStyle(Forme.strainInk).padding(.top, 10)
        }
      }
      .padding(.top, 12)
    }
    SectionTitle(title: "Indices", subtitle: "Calculés sur la dernière mesure")
    FrostCard(radius: 36, padding: 20, spacing: 0) {
      FormeRow(label: "Masse maigre (FFM)", sub: "Poids − masse grasse ; inclut os, eau et organes", value: FF.num(latest.num("derived.ffm_kg"), 1) + " kg")
      FormeDivider(); FormeRow(label: "FFMI", sub: "Masse maigre / taille²", value: FF.num(latest.num("derived.ffmi"), 1) + " kg/m²")
      FormeDivider(); FormeRow(label: "FMI", sub: "Masse grasse / taille²", value: FF.num(latest.num("derived.fmi"), 1) + " kg/m²")
      FormeDivider(); FormeRow(label: "SMI", sub: "Muscle des membres / taille² (appareil)", value: FF.num(latest.num("values.smi"), 1) + " kg/m²")
      FormeDivider(); FormeRow(label: "Muscle / graisse", sub: "Muscle squelettique / masse grasse", value: FF.num(latest.num("derived.smm_to_fat"), 2))
      FormeDivider(); FormeRow(label: "IMC", sub: "Poids / taille²", value: FF.num(latest.num("values.bmi"), 1))
      FormeDivider(); FormeRow(label: "Graisse viscérale (estimée)", sub: "Surface à l'ombilic, modèle de l'appareil", value: FF.num(latest.num("values.vfa_cm2"), 1) + " cm²")
      FormeDivider(); FormeRow(label: "Tour de taille · taille/hanches", value: "\(FF.num(latest.num("values.waist_cm"))) cm · \(FF.num(latest.num("values.whr"), 2))")
      FormeDivider(); FormeRow(label: "Angle de phase", sub: "Intégrité des membranes cellulaires (50 kHz)", value: FF.num(latest.num("values.phase_angle_deg"), 1) + " °")
      FormeDivider(); FormeRow(label: "Masse cellulaire active", value: FF.num(latest.num("values.bcm_kg"), 1) + " kg")
      FormeDivider(); FormeRow(label: "Protéines · minéraux · os", value: "\(FF.num(latest.num("values.protein_kg"), 1)) · \(FF.num(latest.num("values.mineral_kg"), 2)) · \(FF.num(latest.num("values.bmc_kg"), 2)) kg")
      FormeDivider(); FormeRow(label: "Taille utilisée", sub: latest.str("derived.height_source") == "profile" ? "Ton profil" : "Déduite de l'IMC de l'appareil", value: FF.num(latest.num("derived.height_m").map { $0 * 100 }) + " cm")
    }
    .padding(.top, 12)
    TechDetails(subtitle: "Indices propres à l'appareil, hors analyses", rows: [
      ("Score InBody", "Indice propriétaire", FF.num(latest.num("values.inbody_score"))),
      ("Âge InBody", "Indice propriétaire", FF.num(latest.num("values.inbody_age"), 1)),
      ("Niveau de graisse viscérale", "Indice propriétaire", FF.num(latest.num("values.visceral_fat_level"))),
      ("Équilibre haut/bas", "Codes de l'appareil", "\(FF.num(latest.num("values.balance_upper_lower"))) · \(FF.num(latest.num("values.balance_upper"))) · \(FF.num(latest.num("values.balance_lower")))"),
    ]).padding(.top, 32)
  }

  // MARK: muscle

  @ViewBuilder private var muscle: some View {
    let seg = s.obj("segmental")
    SectionTitle(title: "Masse maigre par segment", subtitle: "Dernière mesure · droite / gauche")
    FrostCard(radius: 36, padding: 20, spacing: 14) {
      segRow("Bras", latest.num("values.lean_right_arm_kg"), latest.num("values.lean_left_arm_kg"), latest.obj("derived.arms").num("pct"))
      segRow("Jambes", latest.num("values.lean_right_leg_kg"), latest.num("values.lean_left_leg_kg"), latest.obj("derived.legs").num("pct"))
      FormeRow(label: "Tronc", value: FF.num(latest.num("values.lean_trunk_kg"), 1) + " kg")
      Text(asymText(seg)).font(Forme.sans(12, .medium, relativeTo: .caption)).foregroundStyle(Forme.muted).fixedSize(horizontal: false, vertical: true)
    }
    .padding(.top, 12)
    SectionTitle(title: "Graisse par segment", subtitle: "Dernière mesure")
    FrostCard(radius: 36, padding: 20, spacing: 0) {
      FormeRow(label: "Bras droit · gauche", value: "\(FF.num(latest.num("values.fat_right_arm_kg"), 1)) · \(FF.num(latest.num("values.fat_left_arm_kg"), 1)) kg")
      FormeDivider(); FormeRow(label: "Jambe droite · gauche", value: "\(FF.num(latest.num("values.fat_right_leg_kg"), 1)) · \(FF.num(latest.num("values.fat_left_leg_kg"), 1)) kg")
      FormeDivider(); FormeRow(label: "Tronc", value: FF.num(latest.num("values.fat_trunk_kg"), 1) + " kg")
    }
    .padding(.top, 12)
    let base = seg.obj("vs_baseline")
    if base.obj("right_arm").num("change") != nil {
      SectionTitle(title: "Depuis ta première mesure", subtitle: "Masse maigre par segment")
      FrostCard(radius: 36, padding: 20, spacing: 0) {
        ForEach([("Bras droit", "right_arm"), ("Bras gauche", "left_arm"), ("Tronc", "trunk"), ("Jambe droite", "right_leg"), ("Jambe gauche", "left_leg")], id: \.1) { label, k in
          FormeRow(label: label, value: FF.signed(base.obj(k).num("change"), 2) + " kg")
        }
      }
      .padding(.top, 12)
    }
    chartCard("Muscle squelettique", s.series("series.smm_kg"), "kg", 1)
  }

  // MARK: hydration

  @ViewBuilder private var hydration: some View {
    SectionTitle(title: "Eau corporelle", subtitle: "Contexte de la mesure, pas un diagnostic")
    FrostCard(radius: 36, padding: 20, spacing: 0) {
      FormeRow(label: "Eau totale", value: FF.num(latest.num("values.tbw_l"), 1) + " L")
      FormeDivider(); FormeRow(label: "Intracellulaire · extracellulaire", value: "\(FF.num(latest.num("values.icw_l"), 1)) · \(FF.num(latest.num("values.ecw_l"), 1)) L")
      FormeDivider(); FormeRow(label: "ECW / TBW", sub: "Repère de l'appareil 0,360–0,390", value: FF.num(latest.num("derived.ecw_tbw"), 3))
      FormeDivider(); FormeRow(label: "Eau / masse maigre", sub: "≈ 0,73 attendu", value: FF.num(latest.num("derived.water_per_ffm"), 3))
      FormeDivider(); FormeRow(label: "ECW bras droit · gauche", value: "\(FF.num(latest.num("values.ecw_ratio_right_arm"), 3)) · \(FF.num(latest.num("values.ecw_ratio_left_arm"), 3))")
      FormeDivider(); FormeRow(label: "ECW tronc", value: FF.num(latest.num("values.ecw_ratio_trunk"), 3))
      FormeDivider(); FormeRow(label: "ECW jambe droite · gauche", value: "\(FF.num(latest.num("values.ecw_ratio_right_leg"), 3)) · \(FF.num(latest.num("values.ecw_ratio_left_leg"), 3))")
    }
    .padding(.top, 12)
    let q = latest["derived.quality"] as? [String] ?? []
    FrostCard(radius: 32, padding: 18, spacing: 6) {
      Text("Qualité de la mesure").font(Forme.sans(13, .semibold, relativeTo: .footnote)).foregroundStyle(Forme.ink)
      Text(q.isEmpty ? "Hydratation dans les repères : la répartition muscle / graisse n'est pas faussée par l'eau." : q.joined(separator: "\n"))
        .font(Forme.sans(12, .medium, relativeTo: .caption)).foregroundStyle(q.isEmpty ? Forme.muted : Forme.strainInk).fixedSize(horizontal: false, vertical: true)
      Text("Pour comparer deux mesures : même heure, à jeun, vessie vide, sans sport ni sauna dans les heures avant.").font(Forme.sans(12, .medium, relativeTo: .caption)).foregroundStyle(Forme.muted).fixedSize(horizontal: false, vertical: true)
    }
    .padding(.top, 12)
    chartCard("Eau totale", s.series("series.tbw_l"), "L", 1)
    chartCard("ECW / TBW", s.series("series.ecw_ratio"), "", 3)
  }

  // MARK: metabolism

  @ViewBuilder private var metabolism: some View {
    let m = s.obj("metabolism")
    SectionTitle(title: "Dépense au repos", subtitle: "Trois équations sur la dernière mesure")
    FrostCard(radius: 36, padding: 20, spacing: 0) {
      FormeRow(label: "Katch-McArdle", sub: "370 + 21,6 × masse maigre · utilisée pour ta dépense", value: FF.num(m.num("bmr.katch_mcardle_kcal")) + " kcal")
      FormeDivider(); FormeRow(label: "Cunningham", sub: "500 + 22 × masse maigre", value: FF.num(m.num("bmr.cunningham_kcal")) + " kcal")
      FormeDivider(); FormeRow(label: "Mifflin-St Jeor", sub: "Poids, taille, âge, sexe · sans la bioimpédance", value: FF.num(m.num("bmr.mifflin_st_jeor_kcal")) + " kcal")
      FormeDivider(); FormeRow(label: "Appareil", sub: m["bmr.device_is_katch_mcardle"] as? Bool == true ? "= Katch-McArdle sur sa propre masse maigre" : nil, value: FF.num(m.num("bmr.device_kcal")) + " kcal")
      Text("Katch-McArdle, Cunningham et l'appareil reposent sur la même masse maigre estimée : leur accord ne valide rien. Seul Mifflin-St Jeor est indépendant.")
        .font(Forme.sans(12, .medium, relativeTo: .caption)).foregroundStyle(Forme.muted).fixedSize(horizontal: false, vertical: true).padding(.top, 10)
    }
    .padding(.top, 12)
    SectionTitle(title: "Dépense mesurée au bracelet", subtitle: "Jours portés ≥ 20 h")
    LazyVGrid(columns: [GridItem(.flexible(), spacing: 12), GridItem(.flexible(), spacing: 12)], spacing: 12) {
      MetricTile(label: "Totale · 7 j", value: FF.num(m.num("tdee_7d_kcal")), unit: "kcal/j", note: "28 j : \(FF.num(m.num("tdee_28d_kcal")))")
      MetricTile(label: "Activité · 7 j", value: FF.num(m.num("active_7d_kcal")), unit: "kcal/j", note: "28 j : \(FF.num(m.num("active_28d_kcal")))")
      MetricTile(label: "Par kg", value: FF.num(m.num("tdee_per_kg"), 1), unit: "kcal/kg", note: "\(FF.num(m.num("tdee_per_kg_ffm"), 1)) par kg maigre")
      MetricTile(label: "Niveau d'activité", value: FF.num(m.num("pal"), 2), unit: "× repos", note: "Dépense totale / repos")
    }
    .padding(.top, 12)
    Text("Chaque jour, la dépense au repos utilise la masse maigre de la dernière mesure prise avant ce jour (120 jours au plus), sinon ton profil. Les jours passés gardent leur estimation d'origine.")
      .font(Forme.sans(12, .medium, relativeTo: .caption)).foregroundStyle(Forme.muted).fixedSize(horizontal: false, vertical: true).padding(.top, 10)
  }

  // MARK: adaptation

  @ViewBuilder private var adaptation: some View {
    let a = s.obj("adaptation")
    let horizons = a["horizons"] as? [[String: Any]] ?? []
    SectionTitle(title: "Muscle et graisse", subtitle: "Sur 4, 8 et 12 semaines, avec la marge d'erreur")
    FrostCard(radius: 36, padding: 20, spacing: 0) {
      ForEach(Array(horizons.enumerated()), id: \.offset) { i, h in
        let v = BCVal(h)
        if i > 0 { FormeDivider() }
        FormeRow(label: "\(FF.num(v.num("weeks"))) semaines",
                 sub: v["available"] as? Bool == true ? "Graisse \(change(v.obj("fat")))" : "Il faut 2 mesures couvrant au moins la moitié de la période",
                 value: v["available"] as? Bool == true ? "Muscle \(change(v.obj("smm")))" : "—")
      }
    }
    .padding(.top, 12)
    let vol = a.obj("resistance_volume")
    SectionTitle(title: "Entraînement", subtitle: "Séances de musculation détectées ou lancées")
    LazyVGrid(columns: [GridItem(.flexible(), spacing: 12), GridItem(.flexible(), spacing: 12)], spacing: 12) {
      MetricTile(label: "4 dernières semaines", value: FF.num(vol.num("sessions_last_4w")), unit: "séances", note: "\(FF.num(vol.num("sets_last_4w"))) séries")
      MetricTile(label: "4 semaines avant", value: FF.num(vol.num("sessions_prev_4w")), unit: "séances", note: "\(FF.num(vol.num("sets_prev_4w"))) séries")
      MetricTile(label: "Régularité · 28 j", value: a.num("training_consistency_28d").map { FF.num($0 * 100) } ?? "—", unit: "%", note: "Semaines avec musculation")
    }
    .padding(.top, 12)
    let ctx = latest.obj("context.d28")
    SectionTitle(title: "Avant la dernière mesure", subtitle: "28 jours de bracelet")
    FrostCard(radius: 36, padding: 20, spacing: 0) {
      FormeRow(label: "Sommeil", sub: "Moyenne · écart-type", value: "\(FF.num(ctx.num("sleep_h_mean"), 1)) h · ±\(FF.num(ctx.num("sleep_h_sd"), 1))")
      FormeDivider(); FormeRow(label: "VFC nocturne (RMSSD)", value: FF.num(ctx.num("rmssd_ms_mean")) + " ms")
      FormeDivider(); FormeRow(label: "Pouls de repos", value: FF.num(ctx.num("rhr_bpm_mean")) + " bpm")
      FormeDivider(); FormeRow(label: "Effort moyen", value: FF.num(ctx.num("strain_pct_mean")) + " %")
      FormeDivider(); FormeRow(label: "Séances · temps", value: "\(FF.num(ctx.num("sessions"))) · \(FF.dur(ctx.num("training_min")))")
      FormeDivider(); FormeRow(label: "Jours avec données", value: "\(FF.num(ctx.num("days_with_data"))) / 28")
    }
    .padding(.top, 12)
    let assoc = a["associations"] as? [String: Any] ?? [:]
    let shown = assoc.compactMap { k, v -> (String, Double)? in ((v as? [String: Any])?["spearman"] as? Double).map { (k, $0) } }.sorted { abs($0.1) > abs($1.1) }
    FrostCard(radius: 32, padding: 18, spacing: 6) {
      Text("Liens entre entraînement, sommeil et composition").font(Forme.sans(13, .semibold, relativeTo: .footnote)).foregroundStyle(Forme.ink)
      if shown.isEmpty {
        Text("Calculés à partir de 5 intervalles entre mesures. Ce sont des associations observées, pas des causes.").font(Forme.sans(12, .medium, relativeTo: .caption)).foregroundStyle(Forme.muted).fixedSize(horizontal: false, vertical: true)
      } else {
        ForEach(shown.prefix(6), id: \.0) { k, r in FormeRow(label: assocLabel(k), value: FF.signed(r, 2)) }
        Text("Corrélations de rang (Spearman) entre intervalles : associations, pas des causes.").font(Forme.sans(12, .medium, relativeTo: .caption)).foregroundStyle(Forme.muted)
      }
    }
    .padding(.top, 12)
    chartCard("Graisse", s.series("series.pbf_pct"), "%", 1)
  }

  // MARK: measurements

  @ViewBuilder private var measurements: some View {
    let list = (store.summary["measurements"] as? [[String: Any]] ?? []).reversed()
    SectionTitle(title: "Mesures", subtitle: "Appui long : exclure des tendances ou supprimer")
    FrostCard(radius: 36, padding: 20, spacing: 12) {
      ForEach(Array(list.enumerated()), id: \.offset) { i, m in
        let v = BCVal(m)
        if i > 0 { FormeDivider() }
        FormeRow(label: measuredOn(v) + (v["excluded"] as? Bool == true ? " · exclue" : ""),
                 sub: "\(v.str("device").map { "InBody \($0)" } ?? sourceName(v))\(v.str("local_time").map { " · " + String($0.suffix(8).prefix(5)) } ?? "")",
                 value: "\(FF.num(v.num("values.weight_kg"), 1)) kg · \(FF.num(v.num("values.pbf_pct"), 1)) %",
                 tone: v["excluded"] as? Bool == true ? Forme.muted : Forme.ink)
          .contextMenu {
            let id = v.str("id") ?? ""
            Button(v["excluded"] as? Bool == true ? "Réintégrer aux tendances" : "Exclure des tendances", systemImage: "line.3.horizontal.decrease") { store.setExcluded(id, !(v["excluded"] as? Bool ?? false)) }
            Button("Supprimer", systemImage: "trash", role: .destructive) { pendingDelete = id }
          }
      }
      BodyCompImportButtons(store: store, importing: $importing)
    }
    .padding(.top, 12)
  }

  // MARK: pieces

  @ViewBuilder private func chartCard(_ title: String, _ pts: [(Date, Double)], _ unit: String, _ d: Int) -> some View {
    SectionTitle(title: title, subtitle: pts.count >= 2 ? "\(pts.count) mesures · points observés, rien d'interpolé" : nil)
    FrostCard(radius: 36, padding: 20, spacing: 10) {
      if pts.count >= 2 {
        HRChart(points: pts).frame(height: 110)
        HStack { Text(FF.day(pts[0].0, "d MMM yy")); Spacer(); Text(FF.day(pts[pts.count - 1].0, "d MMM yy")) }
          .font(Forme.sans(12, .medium, relativeTo: .caption)).foregroundStyle(Forme.muted)
      } else {
        HStack {
          Text(pts.isEmpty ? "Pas encore de mesure" : "Une mesure : la courbe apparaît dès la deuxième").font(Forme.sans(13, .regular)).foregroundStyle(Forme.ink2)
          Spacer()
          if let v = pts.last?.1 { NumberUnit(value: FF.num(v, d), unit: unit, size: 22) }
        }
      }
    }
    .padding(.top, 12)
  }

  private func deltaRow(_ label: String, _ v: BCVal, _ unit: String, _ d: Int) -> some View {
    let beyond = v["beyond_error"] as? Bool == true
    return FormeRow(label: label, sub: "Erreur de mesure ± \(FF.num(v.num("mdc95"), 1)) \(unit)", value: v.num("change").map { (beyond ? "" : "≈ ") + FF.signed($0, d) + " " + unit } ?? "—", tone: beyond ? Forme.ink : Forme.muted)
  }

  private func segRow(_ label: String, _ r: Double?, _ l: Double?, _ pct: Double?) -> some View {
    let top = max(r ?? 0, l ?? 0, 0.001)
    return VStack(alignment: .leading, spacing: 6) {
      HStack {
        Text(label).font(Forme.sans(14, .semibold, relativeTo: .subheadline)).foregroundStyle(Forme.ink)
        Spacer()
        Text(pct.map { "écart \(FF.signed($0, 1)) %" } ?? "—").font(Forme.sans(12, .semibold, relativeTo: .caption)).monospacedDigit().foregroundStyle(Forme.muted)
      }
      HStack(spacing: 8) {
        Text("D").font(Forme.sans(11, .bold)).foregroundStyle(Forme.muted).frame(width: 14)
        Track(fraction: (r ?? 0) / top, fill: Forme.ink, height: 10)
        Text(FF.num(r, 2) + " kg").font(Forme.sans(12, .semibold)).monospacedDigit().frame(width: 64, alignment: .trailing)
      }
      HStack(spacing: 8) {
        Text("G").font(Forme.sans(11, .bold)).foregroundStyle(Forme.muted).frame(width: 14)
        Track(fraction: (l ?? 0) / top, fill: Forme.ink2, height: 10)
        Text(FF.num(l, 2) + " kg").font(Forme.sans(12, .semibold)).monospacedDigit().frame(width: 64, alignment: .trailing)
      }
    }
    .accessibilityElement(children: .combine)
  }

  private func asymText(_ seg: BCVal) -> String {
    let persistent = [("bras", seg.obj("arms")), ("jambes", seg.obj("legs"))].filter { $0.1["persistent_asymmetry"] as? Bool == true }.map(\.0)
    if !persistent.isEmpty { return "Écart droite/gauche durable aux \(persistent.joined(separator: " et ")) : plus de 5 % du même côté sur au moins 3 mesures." }
    return "Un écart de quelques % est courant (côté dominant). Il n'est retenu comme durable qu'au-delà de 5 %, du même côté, sur au moins 3 mesures."
  }

  private func change(_ t: BCVal) -> String {
    guard let c = t.num("change") else { return "—" }
    let dir = t.str("direction")
    return "\(dir == "stable" ? "≈ " : "")\(FF.signed(c, 1)) ± \(FF.num(t.num("ci95"), 1)) kg"
  }

  private func recompTitle(_ c: String?) -> String {
    switch c {
    case "muscle_gain_fat_loss": "Muscle en hausse, graisse en baisse"
    case "muscle_gain_stable_fat": "Muscle en hausse, graisse stable"
    case "fat_loss_muscle_preserved": "Graisse en baisse, muscle préservé"
    case "possible_muscle_loss": "Baisse possible du muscle"
    case "indeterminate": "Pas de changement au-delà de l'erreur"
    case "mixed": "Évolution mixte"
    default: "Pas encore de tendance"
    }
  }

  private func recompDetail(_ r: BCVal) -> String {
    if r.str("class") == "insufficient_data" {
      return "\(FF.num(r.num("measurements"))) mesure\((r.num("measurements") ?? 0) > 1 ? "s" : "") sur \(FF.num(r.num("span_days"))) jours. Il en faut 3 sur au moins 28 jours, même appareil et mêmes conditions, pour distinguer un vrai changement de l'erreur de la bioimpédance."
    }
    return "Sur \(FF.num(r.num("measurements"))) mesures et \(FF.num(r.num("span_days"))) jours : muscle \(change(r.obj("muscle"))), graisse \(change(r.obj("fat"))). Une variation n'est retenue que si elle dépasse sa marge d'erreur à 95 %."
  }

  private func assocLabel(_ k: String) -> String {
    let parts = k.split(separator: "~").map(String.init)
    let x: [String: String] = ["strength_sets": "Séries de musculation", "sleep_h_mean": "Sommeil", "rmssd_ms_mean": "VFC", "rhr_bpm_mean": "Pouls de repos", "strain_pct_mean": "Effort"]
    let y: [String: String] = ["smm_per_week": "muscle / semaine", "fat_per_week": "graisse / semaine"]
    return "\(x[parts.first ?? ""] ?? k) ↔ \(y[parts.last ?? ""] ?? "")"
  }
}

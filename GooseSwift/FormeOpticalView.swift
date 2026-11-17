import SwiftUI

/// Respiration and pulse wave from the strap's optical sensor (Rust/core/src/ppg.rs): breathing
/// rate at night (heart-rate variability and PPG, two independent estimates) and at rest during
/// the day, the night pulse-wave template with its features vs the previous nights, SpO2 from the
/// strap, and the audit of the optical channels.
struct FormeOpticalView: View {
  @ObservedObject var labResults: WhoopLabResultsStore

  var body: some View {
    FormeScroll {
      let day = FormeDay(raw: labResults.raw, key: FormeDay.latestNightKey(labResults.raw))
      DetailHeader(title: "Respiration et pouls", chip: day.map { FF.day($0.date, "EEE d MMM") })
      if let day {
        respiration(day)
        pulseWave(day)
        spo2(day)
      }
      FormeNightOpticsCard(store: NightOpticsController.shared)
      audit(J(labResults.raw)["optical_audit"])
    }
    .formeScreen()
  }

  // MARK: respiration

  @ViewBuilder private func respiration(_ day: FormeDay) -> some View {
    let r = day.night["respiration"]
    let ppg = r["ppg"]
    let rest = day.day["respiration_rest"]
    SectionTitle(title: "Respiration", subtitle: "Deux mesures indépendantes la nuit : variabilité cardiaque et onde de pouls")
    HStack(spacing: 12) {
      MetricTile(label: "Nuit", value: FF.num(r["rpm"].d, 1), unit: "resp/min", note: r["rpm_iqr"].arr.count == 2 ? "écart habituel \(FF.num(r["rpm_iqr"][0].d, 1))–\(FF.num(r["rpm_iqr"][1].d, 1))" : nil)
      MetricTile(label: "Onde de pouls", value: FF.num(ppg["rpm"].d, 1), unit: "resp/min", note: agreement(r["ppg_minus_rsa_rpm"].d), noteColor: (r["ppg_minus_rsa_rpm"].d.map { abs($0) <= 1.5 } ?? false) ? Forme.posInk : Forme.muted)
    }
    .padding(.top, 12)
    let nightSeries = series(r["series"])
    if nightSeries.count >= 5 {
      FrostCard(radius: 32, padding: 18, spacing: 8) {
        Text("Pendant le sommeil").font(Forme.sans(13, .semibold, relativeTo: .footnote)).foregroundStyle(Forme.ink2)
        HRChart(points: nightSeries).frame(height: 110)
        Text("Une valeur toutes les minutes (fenêtres de 2 min). Onde de pouls : \(ppg["bursts_agreeing"].i ?? 0) salves concordantes sur \(ppg["bursts"].i ?? 0).")
          .font(Forme.sans(12, .medium, relativeTo: .caption)).foregroundStyle(Forme.muted)
      }
      .padding(.top, 12)
    }
    FrostCard(radius: 32, padding: 18, spacing: 8) {
      HStack(alignment: .firstTextBaseline) {
        Text("Au repos, en journée").font(Forme.sans(13, .semibold, relativeTo: .footnote)).foregroundStyle(Forme.ink2)
        Spacer()
        NumberUnit(value: FF.num(rest["rpm"].d, 1), unit: "resp/min", size: 22)
      }
      let s = series(rest["series"])
      if s.count >= 3 { HRChart(points: s).frame(height: 80) }
      Text(rest["rpm"].d == nil ? "Il faut quelques minutes immobiles pendant les mesures optiques du bracelet (≈ 40 s toutes les 20 min)." : "\(rest["bursts_agreeing"].i ?? 0) mesures immobiles depuis le réveil.")
        .font(Forme.sans(12, .medium, relativeTo: .caption)).foregroundStyle(Forme.muted)
    }
    .padding(.top, 12)
    let trend = day.all.suffix(14).compactMap { d -> (Date, Double)? in
      guard let k = d["date"].s, let date = labDate(k), let v = d["night"]["respiration"]["rpm"].d else { return nil }
      return (date, v)
    }
    if trend.count >= 3 {
      FrostCard(radius: 32, padding: 18, spacing: 8) {
        Text("Nuits récentes").font(Forme.sans(13, .semibold, relativeTo: .footnote)).foregroundStyle(Forme.ink2)
        HRChart(points: trend).frame(height: 80)
        Text("Une hausse durable de plus de 1 à 2 resp/min accompagne souvent une infection ou un surmenage.")
          .font(Forme.sans(12, .medium, relativeTo: .caption)).foregroundStyle(Forme.muted)
      }
      .padding(.top, 12)
    }
  }

  private func agreement(_ d: Double?) -> String? {
    guard let d else { return nil }
    return abs(d) <= 1.5 ? "concorde (\(FF.signed(d, 1)))" : "écart \(FF.signed(d, 1))"
  }

  private func series(_ j: J) -> [(Date, Double)] {
    j.arr.compactMap { p in
      guard let t = p[0].d, let v = p[1].d else { return nil }
      return (Date(timeIntervalSince1970: t), v)
    }
  }

  // MARK: pulse wave

  @ViewBuilder private func pulseWave(_ day: FormeDay) -> some View {
    let w = day.night["pulse_wave"]
    let f = w["features"]
    SectionTitle(title: "Onde de pouls", subtitle: "Forme moyenne d'un battement pendant le sommeil, comparée à tes nuits précédentes")
    FrostCard(radius: 36, padding: 20, spacing: 12) {
      if w["available"].b == true {
        PulseWaveShape(points: w["template"].arr.compactMap { p in p[0].d.flatMap { t in p[1].d.map { (t, $0) } } })
          .frame(height: 130)
          .accessibilityLabel("Forme moyenne du battement, montée en \(FF.num(f["rise_time_ms"].d)) millisecondes")
        Text("\(w["beats"].i ?? 0) battements immobiles moyennés · reproductibilité \(FF.num((w["split_half_r"].d ?? 0) * 100, 1)) %")
          .font(Forme.sans(12, .medium, relativeTo: .caption)).foregroundStyle(Forme.muted)
        let prev = previous(day, { $0["night"]["pulse_wave"]["features"] })
        FormeRow(label: "Montée systolique", sub: "du pied au premier pic", value: ms(f["rise_time_ms"].d, prev.map { $0["rise_time_ms"].d }))
        FormeDivider()
        FormeRow(label: "Onde diastolique", sub: f["diastolic"]["kind"].s == "pic" ? "pic réfléchi après le pic systolique" : "inflexion de la descente", value: ms(f["diastolic"]["delay_ms"].d, prev.map { $0["diastolic"]["delay_ms"].d }))
        FormeDivider()
        FormeRow(label: "Indice de réflexion", sub: "hauteur de l'onde diastolique / systolique", value: ratio(f["diastolic"]["reflection_index"].d, prev.map { $0["diastolic"]["reflection_index"].d }))
        FormeDivider()
        FormeRow(label: "Rapport b/a", sub: "courbure de la montée (dérivée seconde)", value: ratio(f["b_over_a"].d, prev.map { $0["b_over_a"].d }))
        Text("Des indices de forme, suivis d'une nuit à l'autre : ils dépendent aussi du serrage, de la température et de la position du bras, et ne donnent pas une pression artérielle.")
          .font(Forme.sans(12, .medium, relativeTo: .caption)).foregroundStyle(Forme.muted).fixedSize(horizontal: false, vertical: true)
      } else {
        Text(w["reason"].s.map { _ in "Pas assez de battements immobiles cette nuit (\(w["beats"].i ?? 0) sur 60 nécessaires)." } ?? "Pas de mesure optique cette nuit.")
          .font(Forme.sans(14, .regular, relativeTo: .subheadline)).foregroundStyle(Forme.ink2)
      }
    }
    .padding(.top, 12)
  }

  /// the same value on the previous nights (up to 14), for the comparison
  private func previous(_ day: FormeDay, _ pick: (J) -> J) -> [J] {
    day.all.prefix(day.index).suffix(14).map(pick).filter(\.exists)
  }

  private func ms(_ v: Double?, _ prev: [Double?]) -> String {
    guard let v else { return "—" }
    return "\(FF.num(v)) ms" + delta(v, prev, 0, " ms")
  }

  private func ratio(_ v: Double?, _ prev: [Double?]) -> String {
    guard let v else { return "—" }
    return FF.num(v, 2) + delta(v, prev, 2, "")
  }

  private func delta(_ v: Double, _ prev: [Double?], _ d: Int, _ unit: String) -> String {
    let p = prev.compactMap { $0 }
    guard p.count >= 3 else { return "" }
    let m = p.reduce(0, +) / Double(p.count)
    return " (\(FF.signed(v - m, d))\(unit))"
  }

  // MARK: SpO2

  @ViewBuilder private func spo2(_ day: FormeDay) -> some View {
    let s = day.night["spo2"]
    SectionTitle(title: "Saturation en oxygène", subtitle: "Mesurée par le bracelet pendant le sommeil, 30 s toutes les 20 min")
    FrostCard(radius: 32, padding: 18, spacing: 8) {
      HStack(alignment: .firstTextBaseline) {
        Text("Médiane de la nuit").font(Forme.sans(13, .semibold, relativeTo: .footnote)).foregroundStyle(Forme.ink2)
        Spacer()
        NumberUnit(value: FF.num(s["median_pct"].d, 1), unit: "%", size: 22)
      }
      Text(s["available"].b == true ? "\(s["valid_windows"].i ?? 0) mesures · minimum \(FF.num(s["min_window_pct"].d)) % · \(s["windows_below_90"].i ?? 0) sous 90 %" : "Pas de mesure cette nuit.")
        .font(Forme.sans(12, .medium, relativeTo: .caption)).foregroundStyle(Forme.muted)
      Text("Une saturation continue demande une paire rouge + infrarouge mesurée en même temps : le bracelet ne la transmet pas (voir l'audit ci-dessous).")
        .font(Forme.sans(12, .medium, relativeTo: .caption)).foregroundStyle(Forme.muted).fixedSize(horizontal: false, vertical: true)
    }
    .padding(.top, 12)
  }

  // MARK: audit

  @ViewBuilder private func audit(_ a: J) -> some View {
    if a.exists {
      SectionTitle(title: "Capteurs optiques", subtitle: "Ce que chaque canal mesure, d'après les trames enregistrées")
      FrostCard(radius: 36, padding: 20, spacing: 10) {
        ForEach(Array(a["conclusions"].arr.enumerated()), id: \.offset) { _, c in
          Text(c.s ?? "").font(Forme.sans(13, .regular, relativeTo: .footnote)).foregroundStyle(Forme.ink2).fixedSize(horizontal: false, vertical: true)
        }
        let chans = a["channels"].arr.filter { ($0["seconds"].i ?? 0) > 0 }
        ForEach(Array(chans.enumerated()), id: \.offset) { i, c in
          if i == 0 { FormeDivider() }
          let leds = c["leds"].arr.map { "LED \($0["led"].i ?? 0) · \(FF.num($0["ma"].d, 1)) mA" }.joined(separator: " + ")
          FormeRow(label: "Créneau \(c["block"].i ?? 0) → photodiode \(c["photodiode"].i ?? 0)",
                   sub: [leds.isEmpty ? "LED éteintes" : leds, c["class"].s, c["pi_pct"].d.map { "pulsatilité \(FF.num($0, 2)) %" }, c["ua_per_ma"].d.map { "\(FF.num($0, 2)) µA/mA" }].compactMap { $0 }.joined(separator: " · "),
                   value: c["photocurrent_ua"].d.map { "\(FF.num($0, 2)) µA" } ?? "—")
        }
        let roles = a["led_roles"].arr
        if !roles.isEmpty {
          FormeDivider()
          ForEach(Array(roles.enumerated()), id: \.offset) { _, r in
            FormeRow(label: "LED \(r["led"].i ?? 0)", sub: r["evidence"].s, value: r["role"].s ?? "—")
          }
        }
      }
      .padding(.top, 12)
    }
  }
}

/// Normalised beat (foot 0, systolic peak 1) against time from the steepest upstroke.
private struct PulseWaveShape: View {
  let points: [(Double, Double)]
  var body: some View {
    GeometryReader { g in
      let w = g.size.width, h = g.size.height
      let t0 = points.first?.0 ?? -250, t1 = points.last?.0 ?? 850
      let lo = min(points.map(\.1).min() ?? 0, 0), hi = max(points.map(\.1).max() ?? 1, 1)
      let x: (Double) -> CGFloat = { CGFloat(($0 - t0) / max(t1 - t0, 1)) * w }
      let y: (Double) -> CGFloat = { h - CGFloat(($0 - lo) / max(hi - lo, 0.01)) * (h - 8) - 4 }
      ZStack(alignment: .topLeading) {
        Path { p in
          p.move(to: CGPoint(x: x(0), y: 0))
          p.addLine(to: CGPoint(x: x(0), y: h))
        }
        .stroke(Forme.hairline, style: StrokeStyle(lineWidth: 1, dash: [3, 3]))
        Path { p in
          for (i, pt) in points.enumerated() {
            let c = CGPoint(x: x(pt.0), y: y(pt.1))
            if i == 0 { p.move(to: c) } else { p.addLine(to: c) }
          }
        }
        .stroke(Forme.strain, style: StrokeStyle(lineWidth: 2.5, lineCap: .round, lineJoin: .round))
        Text("\(Int(t0)) ms").font(Forme.sans(10, .medium)).foregroundStyle(Forme.muted).position(x: 22, y: h - 6)
        Text("+\(Int(t1)) ms").font(Forme.sans(10, .medium)).foregroundStyle(Forme.muted).position(x: w - 26, y: h - 6)
      }
    }
  }
}

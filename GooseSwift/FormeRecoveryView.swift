import SwiftUI

/// Level 2 · Recovery: score ring, what makes it (z-score per component and its weight), fatigue,
/// morning HRV, trend 7/30/90 days, technical details.
struct FormeRecoveryView: View {
  @ObservedObject var labResults: WhoopLabResultsStore
  let dayKey: String?
  @State private var period = 30

  var body: some View {
    FormeScroll {
      if let day = FormeDay(raw: labResults.raw, key: dayKey) {
        DetailHeader(title: "Récupération", chip: nightChip(day))
        hero(day).padding(.top, 24)
        components(day)
        HStack(spacing: 12) {
          MetricTile(label: "Fatigue", value: FF.num(day.fatigue), unit: "/100", note: day.fatigue.map { $0 < 40 ? "Basse · VFC et charge sur 7 j" : ($0 < 65 ? "Modérée · VFC et charge sur 7 j" : "Élevée · lève le pied") } ?? "Après 5 nuits de référence", noteColor: (day.fatigue ?? 0) < 40 ? Forme.posInk : Forme.muted)
          MetricTile(label: "VFC au réveil", value: FF.num(day.morningHRV), unit: "ms", note: "10 dernières min de sommeil")
        }
        .padding(.top, 12)
        trend(day)
        TechDetails(subtitle: "Variabilité cardiaque, pouls et référence", rows: tech(day)).padding(.top, 32)
      } else {
        DetailHeader(title: "Récupération")
        FormeEmpty(status: labResults.status).padding(.top, 24)
      }
    }
    .formeScreen()
  }

  private func hero(_ day: FormeDay) -> some View {
    FrostCard(radius: 44, padding: 24, spacing: 16) {
      HStack(spacing: 8) {
        FormePill(text: day.recoveryWord, ink: day.recoveryZone == .low ? Forme.muted : Forme.posInk, bg: day.recoveryZone == .low ? Forme.sunken : Forme.posBg)
        FormePill(text: referenceText(day), ink: Forme.ink2, bg: Forme.sunken)
      }
      .frame(maxWidth: .infinity)
      ScoreRing(value: day.recoveryScore, color: Forme.recoveryColor(day.recoveryScore), caption: day.recoveryScore == nil ? "en calibration" : "sur 100")
        .frame(width: 220, height: 220)
        .frame(maxWidth: .infinity)
      Text(day.recoverySentence).font(Forme.sans(15, .regular, relativeTo: .body)).foregroundStyle(Forme.ink2)
        .multilineTextAlignment(.center).frame(maxWidth: 300).frame(maxWidth: .infinity)
    }
  }

  private func referenceText(_ day: FormeDay) -> String {
    guard let n = day.baselineNights else { return "Référence en cours" }
    return day.baselineStatus == "calibrating" ? "Référence · \(n) nuit\(n > 1 ? "s" : "")" : "Référence fiable · \(n) nuits"
  }

  @ViewBuilder private func components(_ day: FormeDay) -> some View {
    let comps = day.indices["recovery"]["components"].dict
    let order: [(String, String, String)] = [
      ("hrv_night", "VFC (variabilité cardiaque) de la nuit", day.hrv.map { "\(FF.num($0)) ms" } ?? "—"),
      ("resting_hr", "Pouls au repos", day.rhr.map { "\(FF.num($0)) bpm" } ?? "—"),
      ("sleep_performance", "Performance du sommeil", day.asleepMin.map { "\(FF.dur($0)) dormies" } ?? "—"),
      ("hrv_morning", "VFC au réveil", day.morningHRV.map { "\(FF.num($0)) ms" } ?? "—"),
      ("hr_recovery_after_effort", "Récupération cardiaque après effort", "—"),
      ("training_form", "Forme d'entraînement (Banister)", day.banister.tsb.map { "TSB \(FF.signed($0, 0))" } ?? "—"),
      ("respiration", "Respiration", day.resp.map { "\(FF.num($0, 1)) /min" } ?? "—"),
    ]
    SectionTitle(title: "Ce qui fait ton score", subtitle: "Écart à ta référence et poids de chaque signal")
    FrostCard(radius: 36, padding: 20, spacing: 0) {
      ForEach(Array(order.enumerated()), id: \.offset) { i, o in
        if i > 0 { FormeDivider() }
        let c = comps[o.0]
        ZBarRow(label: o.1, value: o.2, weight: c?["weight"].d ?? defaultWeight(o.0), z: c?["z"].d)
      }
      Text("À gauche du trait : moins bon que d'habitude. À droite : meilleur.")
        .font(Forme.sans(12, .medium, relativeTo: .caption)).foregroundStyle(Forme.muted).padding(.top, 8)
    }
    .padding(.top, 12)
  }

  private func defaultWeight(_ k: String) -> Double {
    ["hrv_night": 0.30, "hrv_morning": 0.10, "resting_hr": 0.20, "respiration": 0.05, "sleep_performance": 0.15, "hr_recovery_after_effort": 0.10, "training_form": 0.10][k] ?? 0
  }

  @ViewBuilder private func trend(_ day: FormeDay) -> some View {
    let days = day.past(period)
    let values = days.compactMap(\.recoveryScore)
    HStack(alignment: .bottom) {
      SectionTitle(title: "Tendance", subtitle: "Récupération")
      PeriodPicker(period: $period)
    }
    FrostCard(radius: 36, padding: 20, spacing: 12) {
      if values.count >= 2 {
        HStack(alignment: .firstTextBaseline) {
          Text(trendText(values)).font(Forme.sans(15, .regular, relativeTo: .body)).foregroundStyle(Forme.ink2)
          Spacer()
          Text("Moy. \(FF.num(values.reduce(0, +) / Double(values.count)))").font(Forme.sans(13, .semibold, relativeTo: .footnote)).foregroundStyle(Forme.ink)
        }
        Sparkline(values: values, color: Forme.green, lo: 30, hi: 100).frame(height: 100)
          .accessibilityLabel("Récupération sur \(period) jours")
        HStack {
          Text(days.first.map { FF.day($0.date, "d MMM") } ?? "")
          Spacer()
          Text("Aujourd'hui")
        }
        .font(Forme.sans(12, .medium, relativeTo: .caption)).foregroundStyle(Forme.muted)
      } else {
        Text("La courbe apparaît après deux nuits avec un score.").font(Forme.sans(15, .regular, relativeTo: .body)).foregroundStyle(Forme.ink2)
      }
    }
    .padding(.top, 12)
  }

  private func trendText(_ v: [Double]) -> String {
    let d = (v.last ?? 0) - (v.first ?? 0)
    if abs(d) < 3 { return "Stable" }
    return d > 0 ? "En hausse de \(Int(d)) points" : "En baisse de \(Int(-d)) points"
  }

  private func tech(_ day: FormeDay) -> [(String, String, String)] {
    let n = day.night
    return [
      ("RMSSD nuit entière", "Moyenne quadratique des différences successives entre battements", n["hrv_night"]["rmssd_ms"].d.map { "\(FF.num($0)) ms" } ?? "—"),
      ("RMSSD dernier tiers de nuit", "Moins sensible au début de nuit", n["hrv_last_third"]["rmssd_ms"].d.map { "\(FF.num($0)) ms" } ?? "—"),
      ("ln RMSSD", "Logarithme utilisé pour les z-scores", FF.num(n["hrv_night"]["ln_rmssd"].d, 2)),
      ("SDNN", "Écart-type des intervalles entre battements", n["hrv_night"]["sdnn_ms"].d.map { "\(FF.num($0)) ms" } ?? "—"),
      ("pNN50", "Battements successifs écartés de plus de 50 ms", n["hrv_night"]["pnn50_pct"].d.map { "\(FF.num($0)) %" } ?? "—"),
      ("Artefacts écartés", "Contrôle qualité des intervalles R-R", n["hrv_qc"]["artifact_pct"].d.map { "\(FF.num($0, 1)) %" } ?? "—"),
      ("Pouls au repos du bracelet", "10e centile pendant le sommeil", n["firmware_resting_hr"]["p10_bpm"].d.map { "\(FF.num($0)) bpm" } ?? "—"),
      ("Pouls moyen de la nuit", "Pendant le sommeil", n["mean_sleep_hr_bpm"].d.map { "\(FF.num($0)) bpm" } ?? "—"),
      ("z VFC · z pouls au repos", "Écart à ta référence, en écarts-types", "\(FF.signed(n["baselines"]["hrv_rmssd"]["z"].d, 1)) · \(FF.signed(n["baselines"]["resting_hr"]["z"].d, 1))"),
      ("Référence", "Moyenne lissée de tes nuits", day.baselineStatus == "calibrating" ? "En calibration" : "Fiable"),
    ]
  }
}

/// Ring with a dot-matrix score in the middle.
struct ScoreRing: View {
  let value: Double?
  var color: Color = Forme.green
  var caption: String = "sur 100"
  var body: some View {
    ZStack {
      Circle().stroke(Forme.sunken, lineWidth: 14).padding(7)
      Circle().trim(from: 0, to: CGFloat((value ?? 0) / 100))
        .stroke(color, style: StrokeStyle(lineWidth: 14, lineCap: .round))
        .rotationEffect(.degrees(-90))
        .padding(7)
      VStack(spacing: 6) {
        DotNumber(value: value.map { FF.num($0) }, size: 64).foregroundStyle(Forme.ink)
        Text(caption).font(Forme.sans(12, .semibold, relativeTo: .caption)).foregroundStyle(Forme.muted)
      }
    }
    .accessibilityElement(children: .ignore)
    .accessibilityLabel(value.map { "\(Int($0)) sur 100" } ?? "Score en calibration")
  }
}

/// A component row: label, value, weight, centred z-bar (−2…+2 σ).
struct ZBarRow: View {
  let label: String
  let value: String
  let weight: Double
  let z: Double?
  var body: some View {
    VStack(alignment: .leading, spacing: 8) {
      HStack(alignment: .firstTextBaseline, spacing: 8) {
        Text(label).font(Forme.sans(14, .semibold, relativeTo: .subheadline)).foregroundStyle(Forme.ink).frame(maxWidth: .infinity, alignment: .leading)
        Text(value).font(Forme.sans(15, .semibold, relativeTo: .subheadline)).monospacedDigit().foregroundStyle(Forme.ink).lineLimit(1)
      }
      HStack(spacing: 10) {
        Text("×\(FF.num(weight, 2))").font(Forme.sans(12, .medium, relativeTo: .caption)).foregroundStyle(Forme.muted).frame(width: 44, alignment: .leading)
        GeometryReader { g in
          let w = g.size.width
          let zz = max(-2, min(2, z ?? 0))
          let half = CGFloat(abs(zz) / 2) * w / 2
          ZStack(alignment: .leading) {
            Capsule().fill(Forme.sunken)
            Capsule().fill(zz > 0.05 ? Forme.green : (zz < -0.05 ? Forme.muted : Forme.dotOff))
              .frame(width: max(half, 4))
              .offset(x: zz >= 0 ? w / 2 : w / 2 - half)
              .opacity(z == nil ? 0 : 1)
            Rectangle().fill(Forme.hex(0xaab3bb)).frame(width: 2, height: 16).offset(x: w / 2 - 1)
          }
        }
        .frame(height: 10)
        Text(z.map { "\(FF.signed($0, 1)) σ" } ?? "—").font(Forme.sans(12, .semibold, relativeTo: .caption)).monospacedDigit()
          .foregroundStyle((z ?? 0) > 0.05 ? Forme.posInk : Forme.muted).frame(width: 64, alignment: .trailing)
      }
    }
    .padding(.vertical, 14)
    .accessibilityElement(children: .ignore)
    .accessibilityLabel("\(label), \(value), écart \(z.map { FF.num($0, 1) } ?? "inconnu") écart-type, poids \(FF.num(weight, 2))")
  }
}

struct FormeEmpty: View {
  let status: String
  var body: some View {
    FrostCard(radius: 36, padding: 24, spacing: 8) {
      Text("Pas encore de données").font(Forme.sans(20, .semibold, relativeTo: .title3)).foregroundStyle(Forme.ink)
      Text("Porte le bracelet et laisse la synchronisation se faire. \(status)").font(Forme.sans(15, .regular, relativeTo: .body)).foregroundStyle(Forme.ink2)
    }
  }
}

import Charts
import SwiftUI

// whoop-lab: the "Aujourd'hui" day view. Order: Activité, Nuit, Corps. Each section is a card with
// its headline score and plain-language status so the day reads at a glance.

// MARK: - Shared look

enum LabStage: Character, CaseIterable {
  case wake = "W", rem = "R", light = "L", deep = "D"

  var key: String { ["W": "wake", "R": "rem", "L": "light", "D": "deep"][String(rawValue)]! }
  var label: String {
    switch self {
    case .wake: "Éveil"
    case .rem: "Paradoxal"
    case .light: "Léger"
    case .deep: "Profond"
    }
  }
  var color: Color {
    switch self {
    case .wake: Color(red: 0.98, green: 0.45, blue: 0.40)
    case .rem: Color(red: 0.62, green: 0.48, blue: 0.95)
    case .light: Color(red: 0.40, green: 0.70, blue: 0.98)
    case .deep: Color(red: 0.17, green: 0.32, blue: 0.80)
    }
  }
  /// Lane from top (wake) to bottom (deep), as in clinical hypnograms.
  var lane: Int { [.wake: 0, .rem: 1, .light: 2, .deep: 3][self]! }
}

struct LabStatus {
  let text: String
  let color: Color

  static let good = LabStatus(text: "Bon", color: .green)
  static let fair = LabStatus(text: "Moyen", color: .orange)
  static let low = LabStatus(text: "À surveiller", color: .red)
  static let neutral = LabStatus(text: "", color: .secondary)
}

struct LabCard<Content: View>: View {
  let title: String
  let systemImage: String
  let tint: Color
  var badge: String = ""
  var badgeCaption: String = ""
  @ViewBuilder var content: Content

  var body: some View {
    VStack(alignment: .leading, spacing: 14) {
      HStack(alignment: .center) {
        Label(title, systemImage: systemImage)
          .font(.title3.weight(.bold))
          .foregroundStyle(tint)
        Spacer()
        if !badge.isEmpty {
          VStack(alignment: .trailing, spacing: 0) {
            Text(badge).font(.title2.weight(.bold).monospacedDigit())
            if !badgeCaption.isEmpty {
              Text(badgeCaption).font(.caption2).foregroundStyle(.secondary)
            }
          }
        }
      }
      content
    }
    .padding(16)
    .background(.background.secondary, in: RoundedRectangle(cornerRadius: 20))
  }
}

/// Big value + unit + short status, sized for a glance.
struct LabMetric: View {
  let title: String
  let value: String
  var unit: String = ""
  var status: LabStatus = .neutral
  var detail: String = ""

  var body: some View {
    VStack(alignment: .leading, spacing: 4) {
      Text(title).font(.subheadline).foregroundStyle(.secondary)
      HStack(alignment: .firstTextBaseline, spacing: 4) {
        Text(value).font(.system(size: 28, weight: .semibold, design: .rounded)).monospacedDigit()
        if !unit.isEmpty { Text(unit).font(.subheadline).foregroundStyle(.secondary) }
      }
      if !status.text.isEmpty {
        Text(status.text)
          .font(.caption.weight(.semibold))
          .padding(.horizontal, 8).padding(.vertical, 2)
          .background(status.color.opacity(0.15), in: Capsule())
          .foregroundStyle(status.color)
      }
      if !detail.isEmpty {
        Text(detail).font(.caption).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
      }
    }
    .frame(maxWidth: .infinity, alignment: .leading)
  }
}

private let twoColumns = [GridItem(.flexible(), spacing: 16, alignment: .top), GridItem(.flexible(), spacing: 16, alignment: .top)]

// MARK: - Activité

struct LabActivityCard: View {
  let day: LabDay
  private let stepGoal = 8_000.0 // mortality benefit plateaus around 8-10k steps/day (Paluch 2022)

  var body: some View {
    LabCard(title: "Activité", systemImage: "figure.walk", tint: .orange,
            badge: day.strain021.map { labFmt($0, 1) } ?? "—", badgeCaption: "effort / 21") {
      if day.hasActivity {
        VStack(alignment: .leading, spacing: 6) {
          HStack(alignment: .firstTextBaseline) {
            Text((day.steps ?? 0).formatted()).font(.system(size: 34, weight: .bold, design: .rounded)).monospacedDigit()
            Text("pas").foregroundStyle(.secondary)
            Spacer()
            Text("objectif \(Int(stepGoal).formatted())").font(.caption).foregroundStyle(.secondary)
          }
          ProgressView(value: min(1, Double(day.steps ?? 0) / stepGoal)).tint(.orange)
        }
        LazyVGrid(columns: twoColumns, spacing: 16) {
          LabMetric(title: "Calories actives", value: labFmt(day.energy?.activeKcal), unit: "kcal",
                    detail: "dépense totale \(labFmt(day.energy?.totalKcal)) kcal")
          LabMetric(title: "Activité modérée+", value: labFmt(day.mvpaMin), unit: "min",
                    status: (day.mvpaMin ?? 0) >= 22 ? .good : (day.mvpaMin ?? 0) >= 10 ? .fair : .neutral,
                    detail: "repère OMS ≈ 22 min/jour")
          LabMetric(title: "Stress", value: labFmt(day.stress?.mean), unit: "/100",
                    status: stressStatus, detail: "éveillé, au repos")
          LabMetric(title: "FC moyenne", value: labFmt(day.hrMean), unit: "bpm",
                    detail: "\(labFmt(day.hrMin)) – \(labFmt(day.hrMax)) bpm")
          if let cad = day.cadence, cad.available {
            LabMetric(title: "Cadence de marche", value: labFmt(cad.medianSpm), unit: "pas/min",
                      detail: "rapide : \(labFmt(cad.p90Spm)) pas/min")
          }
        }
        if let hourly = day.hourly, !hourly.isEmpty {
          LabHourlyChart(hourly: hourly)
        }
        if day.energy?.profileDefaultsUsed == true {
          Label("Renseigne âge et poids dans ton profil pour des calories exactes.", systemImage: "info.circle")
            .font(.caption).foregroundStyle(.secondary)
        }
      } else {
        Text("Pas assez de données ce jour (\(labFmt(day.coverageH, 1)) h portées).").foregroundStyle(.secondary)
      }
    }
  }

  private var stressStatus: LabStatus {
    guard let mean = day.stress?.mean else { return .neutral }
    return mean <= 25 ? LabStatus(text: "Calme", color: .green) : mean <= 50 ? LabStatus(text: "Bas", color: .teal)
      : mean <= 75 ? .fair : .low
  }
}

struct LabHourlyChart: View {
  let hourly: [String: LabDay.Hour]

  var body: some View {
    let points = hourly.compactMap { key, value in Int(key).map { ($0, value) } }.sorted { $0.0 < $1.0 }
    VStack(alignment: .leading, spacing: 4) {
      Text("Pas et fréquence cardiaque par heure").font(.caption).foregroundStyle(.secondary)
      Chart {
        ForEach(points, id: \.0) { hour, value in
          BarMark(x: .value("Heure", hour), y: .value("Pas", value.steps))
            .foregroundStyle(.orange.opacity(0.75))
        }
      }
      .chartXScale(domain: 0...23)
      .chartXAxis { AxisMarks(values: [0, 6, 12, 18, 23]) { v in AxisValueLabel { Text("\(v.as(Int.self) ?? 0)h") } } }
      .frame(height: 80)
      Chart {
        ForEach(points, id: \.0) { hour, value in
          LineMark(x: .value("Heure", hour), y: .value("FC", value.hr)).foregroundStyle(.red).interpolationMethod(.catmullRom)
        }
      }
      .chartXScale(domain: 0...23)
      .chartYScale(domain: .automatic(includesZero: false))
      .chartXAxis(.hidden)
      .frame(height: 60)
    }
  }
}

// MARK: - Nuit

struct LabNightCard: View {
  let night: LabNight

  var body: some View {
    LabCard(title: "Nuit", systemImage: "moon.stars.fill", tint: .indigo,
            badge: night.sleepScore.map { "\($0)%" } ?? "—", badgeCaption: "de 8 h de sommeil") {
      if night.available {
        LabSleepTimeline(night: night)
        LazyVGrid(columns: twoColumns, spacing: 16) {
          LabMetric(title: "Temps de sommeil", value: labDuration(night.asleepMin),
                    status: (night.asleepMin ?? 0) >= 420 ? .good : (night.asleepMin ?? 0) >= 360 ? .fair : .low,
                    detail: "recommandé : 7 h ou plus")
          LabMetric(title: "Efficacité", value: labFmt(night.efficiencyPct), unit: "%",
                    status: (night.efficiencyPct ?? 0) >= 85 ? .good : (night.efficiencyPct ?? 0) >= 75 ? .fair : .low,
                    detail: "\(night.awakenings ?? 0) réveils")
        }
        if let codes = night.hypnogram, let start = night.hypnogramStart {
          LabHypnogramChart(codes: Array(codes), start: start)
          LabStageBreakdown(stages: night.stagesMin ?? [:])
        }
      } else {
        Text("Aucune nuit détectée : bracelet non porté ou pas de sommeil enregistré.").foregroundStyle(.secondary)
      }
    }
  }
}

/// Couché -> endormi -> réveil on one line, with the time to fall asleep shaded.
struct LabSleepTimeline: View {
  let night: LabNight

  var body: some View {
    VStack(spacing: 8) {
      HStack(alignment: .top) {
        moment("Couché", night.bedTime, icon: "bed.double.fill", alignment: .leading)
        Spacer()
        moment("Endormi", night.sleepOnset, icon: "moon.zzz.fill", alignment: .center)
        Spacer()
        moment("Réveil", night.wakeTime, icon: "sun.max.fill", alignment: .trailing)
      }
      GeometryReader { geo in
        let total = max(1, (night.wakeTime ?? 0) - (night.bedTime ?? 0))
        let latency = max(0, (night.sleepOnset ?? 0) - (night.bedTime ?? 0))
        let w = geo.size.width
        ZStack(alignment: .leading) {
          Capsule().fill(Color.indigo.opacity(0.85))
          Capsule().fill(Color.gray.opacity(0.35)).frame(width: max(6, w * latency / total))
        }
      }
      .frame(height: 10)
      HStack {
        Text("Endormi en \(labFmt(night.latencyMin)) min").font(.caption).foregroundStyle(.secondary)
        Spacer()
        Text("Au lit \(labDuration(night.timeInBedMin))").font(.caption).foregroundStyle(.secondary)
      }
    }
  }

  private func moment(_ title: String, _ ts: Double?, icon: String, alignment: HorizontalAlignment) -> some View {
    VStack(alignment: alignment, spacing: 2) {
      Label(title, systemImage: icon).font(.caption).foregroundStyle(.secondary)
      Text(labClock(ts)).font(.system(size: 24, weight: .semibold, design: .rounded)).monospacedDigit()
    }
  }
}

/// Stepped hypnogram: one lane per stage (éveil on top, profond at the bottom), rounded blocks for
/// each continuous stage period, thin connectors between periods and hour ticks.
struct LabHypnogramChart: View {
  let codes: [Character]
  let start: Double

  private struct Run { let stage: LabStage; let from: Int; let to: Int }

  private var runs: [Run] {
    var out: [Run] = []
    for (i, c) in codes.enumerated() {
      guard let stage = LabStage(rawValue: c) else { continue }
      if let last = out.last, last.stage == stage, last.to == i {
        out[out.count - 1] = Run(stage: stage, from: last.from, to: i + 1)
      } else {
        out.append(Run(stage: stage, from: i, to: i + 1))
      }
    }
    return out
  }

  var body: some View {
    HStack(alignment: .top, spacing: 8) {
      VStack(alignment: .trailing, spacing: 0) {
        ForEach(LabStage.allCases, id: \.self) { stage in
          Text(stage.label).font(.caption2).foregroundStyle(stage.color)
            .frame(height: 28)
        }
      }
      VStack(spacing: 4) {
        Canvas { context, size in
          let n = CGFloat(max(codes.count, 1))
          let lane = size.height / 4
          // lane guides
          for l in 0..<4 {
            let y = CGFloat(l) * lane + lane / 2
            context.stroke(Path { p in p.move(to: CGPoint(x: 0, y: y)); p.addLine(to: CGPoint(x: size.width, y: y)) },
                           with: .color(.secondary.opacity(0.12)), lineWidth: 1)
          }
          let all = runs
          for (index, run) in all.enumerated() {
            let x0 = CGFloat(run.from) / n * size.width
            let x1 = CGFloat(run.to) / n * size.width
            let y = CGFloat(run.stage.lane) * lane
            let rect = CGRect(x: x0, y: y + 5, width: max(x1 - x0, 1.5), height: lane - 10)
            context.fill(Path(roundedRect: rect, cornerRadius: min(4, rect.width / 2)), with: .color(run.stage.color))
            if index + 1 < all.count {
              let next = all[index + 1]
              let ya = y + lane / 2
              let yb = CGFloat(next.stage.lane) * lane + lane / 2
              context.stroke(Path { p in p.move(to: CGPoint(x: x1, y: ya)); p.addLine(to: CGPoint(x: x1, y: yb)) },
                             with: .color(.secondary.opacity(0.35)), lineWidth: 1)
            }
          }
        }
        .frame(height: 112)
        hourTicks
      }
    }
    .accessibilityElement(children: .ignore)
    .accessibilityLabel("Hypnogramme de la nuit")
  }

  private var hourTicks: some View {
    GeometryReader { geo in
      let duration = Double(codes.count) * 30
      let firstHour = (start / 3600).rounded(.up) * 3600
      let hours = stride(from: firstHour, through: start + duration, by: 3600).map { $0 }
      let step = max(1, hours.count / 5)
      ZStack(alignment: .topLeading) {
        ForEach(Array(hours.enumerated()), id: \.offset) { i, h in
          if i % step == 0 {
            Text(labClock(h)).font(.caption2).foregroundStyle(.secondary)
              .position(x: geo.size.width * (h - start) / duration, y: 6)
          }
        }
      }
    }
    .frame(height: 14)
  }
}

/// Proportion bar + minutes and share of each stage.
struct LabStageBreakdown: View {
  let stages: [String: Double]

  var body: some View {
    let total = max(1, LabStage.allCases.reduce(0) { $0 + (stages[$1.key] ?? 0) })
    VStack(alignment: .leading, spacing: 8) {
      GeometryReader { geo in
        HStack(spacing: 2) {
          ForEach(LabStage.allCases.reversed(), id: \.self) { stage in
            let share = (stages[stage.key] ?? 0) / total
            if share > 0 {
              RoundedRectangle(cornerRadius: 3).fill(stage.color).frame(width: max(2, geo.size.width * share - 2))
            }
          }
        }
      }
      .frame(height: 10)
      HStack(spacing: 0) {
        ForEach(LabStage.allCases.reversed(), id: \.self) { stage in
          VStack(alignment: .leading, spacing: 1) {
            HStack(spacing: 4) {
              Circle().fill(stage.color).frame(width: 8, height: 8)
              Text(stage.label).font(.caption)
            }
            Text(labDuration(stages[stage.key])).font(.subheadline.weight(.semibold)).monospacedDigit()
            Text("\(Int(((stages[stage.key] ?? 0) / total * 100).rounded())) %").font(.caption2).foregroundStyle(.secondary)
          }
          .frame(maxWidth: .infinity, alignment: .leading)
        }
      }
      Text("Stades estimés sans EEG à partir du cœur et du mouvement ; le sommeil profond est l'estimation la moins sûre.")
        .font(.caption2).foregroundStyle(.secondary)
    }
  }
}

// MARK: - Corps

struct LabBodyCard: View {
  let night: LabNight

  var body: some View {
    LabCard(title: "Corps", systemImage: "heart.fill", tint: .red,
            badge: night.recovery?.score.map { "\(Int($0))%" } ?? "—",
            badgeCaption: night.recovery?.score == nil ? (night.recovery?.reason ?? "récupération") : "récupération") {
      if night.available {
        LazyVGrid(columns: twoColumns, spacing: 16) {
          LabMetric(title: "FC au repos", value: labFmt(night.restingHrBpm), unit: "bpm",
                    status: rangeStatus(night.restingHrBpm, good: 40...60, fair: 60...70),
                    detail: night.firmwareRestingHr?.p10Bpm.map { "la plus basse du sommeil · bracelet : \(labFmt($0)) bpm" } ?? "la plus basse du sommeil")
          LabMetric(title: "VFC", value: labFmt(night.hrvNight?.rmssdMs), unit: "ms",
                    detail: "RMSSD nocturne ; à comparer à ta moyenne")
          LabMetric(title: "Respiration", value: labFmt(night.respiration?.rpm, 1), unit: "/min",
                    status: rangeStatus(night.respiration?.rpm, good: 12...20, fair: 10...22),
                    detail: "normal au repos : 12–20")
          LabMetric(title: "Température peau", value: night.skinTempDeltaC.map { String(format: "%+.1f", $0) } ?? labFmt(night.skinTempC, 1),
                    unit: "°C",
                    status: night.skinTempDeltaC.map { abs($0) <= 0.5 ? .good : abs($0) <= 1 ? .fair : .low } ?? .neutral,
                    detail: night.skinTempDeltaC != nil ? "écart à ta moyenne" : "écart disponible après 3 nuits")
          if let amp = night.perfusion?.opticalAmplitudeMedian {
            LabMetric(title: "Perfusion", value: night.perfusion?.vsPreviousNightsPct.map { String(format: "%+.0f", $0) } ?? labFmt(amp),
                      unit: night.perfusion?.vsPreviousNightsPct != nil ? "%" : "u.r.",
                      detail: night.perfusion?.vsPreviousNightsPct != nil ? "amplitude du pouls vs ta moyenne " : "amplitude du pouls ; écart après 3 nuits")
          }
          if let spo2 = night.spo2, spo2.available {
            LabMetric(title: "SpO₂ (bracelet)", value: labFmt(spo2.medianPct), unit: "%",
                      status: rangeStatus(spo2.medianPct, good: 95...100, fair: 90...95),
                      detail: "mesure du bracelet, \(spo2.validWindows ?? 0)/\(spo2.windows ?? 0) relevés ; min \(labFmt(spo2.minWindowPct)) %")
          }
        }
      } else {
        Text("Ces signaux sont mesurés pendant le sommeil.").foregroundStyle(.secondary)
      }
    }
  }

  private func rangeStatus(_ value: Double?, good: ClosedRange<Double>, fair: ClosedRange<Double>) -> LabStatus {
    guard let value else { return .neutral }
    return good.contains(value) ? .good : fair.contains(value) ? .fair : .low
  }
}

// MARK: - Detail screen

/// Detail screen for a metric family: the matching card for the latest day, then the history.
struct LabRouteDetailView: View {
  let route: HealthRoute
  let results: LabResults

  var body: some View {
    ScrollView {
      VStack(alignment: .leading, spacing: 20) {
        if let day = results.latest {
          switch route {
          case .sleep:
            LabNightCard(night: day.night)
          case .recovery, .healthMonitor:
            LabBodyCard(night: day.night)
          case .strain, .stress, .cardioLoad, .energyBank:
            LabActivityCard(day: day.day)
            if let load = results.load { LabLoadSection(load: load) }
          default:
            EmptyView()
          }
        }
        LabTrendsView(results: results)
      }
      .padding()
    }
    .navigationTitle(route.title)
  }
}

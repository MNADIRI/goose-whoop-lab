"""Render the analyses as a self-contained, interpretable HTML report (French).

    python3 report.py            -> ingest new data, analyse, write report.html
"""

from __future__ import annotations

import datetime as dt
import html
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import analyze  # noqa: E402

STAGE_COLORS = {"wake": "var(--wake)", "rem": "var(--rem)", "light": "var(--light)", "deep": "var(--deep)"}
STAGE_FR = {"wake": "Éveil", "rem": "Paradoxal", "light": "Léger", "deep": "Profond"}


def hm(ts: int | None) -> str:
    return dt.datetime.fromtimestamp(ts).strftime("%H:%M") if ts else "—"


def dur(minutes: float | None) -> str:
    if minutes is None:
        return "—"
    return f"{int(minutes // 60)} h {int(minutes % 60):02d}"


def card(label: str, value: str, note: str = "", status: str = "") -> str:
    return (f'<div class="card {status}"><div class="label">{html.escape(label)}</div>'
            f'<div class="value">{value}</div><div class="note">{note}</div></div>')


def hypnogram(stages: list[str], onset: int) -> str:
    if not stages:
        return ""
    w, h = 720, 90
    rows = {"wake": 0, "rem": 1, "light": 2, "deep": 3}
    step = w / len(stages)
    rects = "".join(
        f'<rect x="{i * step:.2f}" y="{rows[s] * 20 + 4}" width="{step + 0.3:.2f}" height="16" fill="{STAGE_COLORS[s]}"/>'
        for i, s in enumerate(stages))
    labels = "".join(f'<text x="-6" y="{rows[s] * 20 + 16}" text-anchor="end">{STAGE_FR[s]}</text>' for s in rows)
    ticks = ""
    for k in range(0, len(stages), 120):  # every hour
        ticks += f'<text x="{k * step:.1f}" y="{h + 4}" text-anchor="middle">{hm(onset + k * 30)}</text>'
    return (f'<svg viewBox="-70 0 {w + 80} {h + 14}" class="chart" role="img" aria-label="Hypnogramme">'
            f'{labels}{rects}{ticks}</svg>')


def bars(values: dict[int, float], unit: str, label: str) -> str:
    if not values:
        return ""
    w, h = 720, 120
    vmax = max(values.values()) or 1
    bw = w / 24
    out = ""
    for hour in range(24):
        v = values.get(hour)
        if v is None:
            continue
        bh = (v / vmax) * (h - 20)
        out += (f'<rect x="{hour * bw + 2:.1f}" y="{h - bh:.1f}" width="{bw - 4:.1f}" height="{bh:.1f}" fill="var(--accent)">'
                f'<title>{hour}h : {v:g} {unit}</title></rect>')
    ticks = "".join(f'<text x="{hh * bw + bw / 2:.1f}" y="{h + 14}" text-anchor="middle">{hh}h</text>' for hh in range(0, 24, 3))
    return f'<figure><figcaption>{label}</figcaption><svg viewBox="0 0 {w} {h + 18}" class="chart">{out}{ticks}</svg></figure>'


def night_label(date: str) -> str:
    d = dt.date.fromisoformat(date)
    return f"{d.day} au {(d + dt.timedelta(days=1)).strftime('%d/%m/%Y').lstrip('0')}"


def interpret_night(n: dict) -> list[str]:
    tips = []
    if n["asleep_min"] < 420:
        tips.append(f"Sommeil de {dur(n['asleep_min'])} : sous les 7 h recommandées pour un adulte (AASM/SRS).")
    else:
        tips.append(f"Sommeil de {dur(n['asleep_min'])} : dans la recommandation de 7 h ou plus.")
    eff = n["efficiency_pct"]
    tips.append(f"Efficacité {eff:.0f} % : {'bonne (≥ 85 %)' if eff >= 85 else 'à améliorer (< 85 %)'}.")
    hrv = n.get("hrv_night")
    if hrv:
        tips.append(f"VFC nocturne RMSSD {hrv['rmssd_ms']:.0f} ms (ln {hrv['ln_rmssd']:.2f}). "
                    "Valeur à comparer à ta propre moyenne : l'écart compte plus que le chiffre absolu.")
    tips.append(f"FC au repos {n['resting_hr_bpm']:.0f} bpm (plus basse moyenne glissante de 5 min pendant le sommeil).")
    return tips


def render(result: dict) -> str:
    nights = [n for n in result["nights"] if n.get("available")]
    days = [d for d in result["days"] if d.get("steps") is not None]
    dev = result["device"]
    night = nights[-1] if nights else None
    sections = []

    if night:
        hrv = night.get("hrv_night") or {}
        last = night.get("hrv_last_third") or {}
        resp = night.get("respiration") or {}
        rec = night.get("recovery") or {}
        st = night["stages_min"]
        cards = "".join([
            card("Sommeil", dur(night["asleep_min"]), f"{hm(night['sleep_onset'])} → {hm(night['wake_time'])} · au lit {dur(night['time_in_bed_min'])}"),
            card("Efficacité", f"{night['efficiency_pct']:.0f} %", f"latence {night['latency_min']} min · {night['awakenings']} réveils"),
            card("FC au repos", f"{night['resting_hr_bpm']:.0f} <small>bpm</small>", f"moyenne nuit {night['mean_sleep_hr_bpm']:.0f} bpm"),
            card("VFC (RMSSD)", f"{hrv.get('rmssd_ms', '—')} <small>ms</small>",
                 f"dernier tiers {last.get('rmssd_ms', '—')} ms · SDNN {hrv.get('sdnn_ms', '—')} ms · {hrv.get('successive_diffs', 0)} paires"),
            card("Respiration", f"{resp.get('rpm', '—')} <small>/min</small>",
                 f"IQR {resp.get('rpm_iqr', ['—', '—'])[0]}–{resp.get('rpm_iqr', ['—', '—'])[1]} · {resp.get('windows', 0)} fenêtres" if resp else "pas assez de battements continus"),
            card("Peau (sommeil)", f"{night['skin_temp_c']} <small>°C</small>",
                 f"écart {night['skin_temp_delta_c']:+.2f} °C" if night.get("skin_temp_delta_c") is not None else "référence en cours (3 nuits)"),
            card("Récupération", f"{rec.get('score') if rec.get('score') is not None else '—'}",
                 rec.get("reason") or f"z ln RMSSD {rec.get('z_ln_rmssd')} · z FC repos {rec.get('z_resting_hr')}", "muted" if rec.get("score") is None else ""),
        ])
        legend = " ".join(f'<span class="dot" style="background:{STAGE_COLORS[s]}"></span>{STAGE_FR[s]} {st[s]} min' for s in ("wake", "rem", "light", "deep"))
        tips = "".join(f"<li>{html.escape(t)}</li>" for t in interpret_night(night))
        sections.append(f"""<section><h2>Nuit du {night_label(night['date'])}</h2><div class="grid">{cards}</div>
<figure><figcaption>Hypnogramme (époques de 30 s) — {legend}</figcaption>{hypnogram(night['hypnogram'], night['sleep_onset'])}</figure>
<ul class="tips">{tips}</ul>
<p class="caveat">Les stades sont estimés à partir de la FC, de la VFC locale et du mouvement (règles transparentes, sans EEG).
La séparation léger/profond est la moins fiable : la littérature plafonne à ~65–73 % d'accord par époque pour ce type de méthode.</p></section>""")

    if days:
        d = days[-1]
        e = d["energy"]
        z = d["hr_zone_min"]
        zone_txt = " · ".join(f"Z{k[1]} {v:g}" for k, v in z.items())
        cards = "".join([
            card("Pas", f"{d['steps']:,}".replace(",", " "), f"marche {d['walk_min']:g} min · course {d['run_min']:g} min"),
            card("Activité modérée+", f"{d['mvpa_min']:g} <small>min</small>", "≥ 40 % de la réserve cardiaque (OMS : 150 min/semaine)"),
            card("Charge cardio", f"{d['strain_0_21']:g} <small>/21</small>", f"TRIMP Edwards {d['edwards_trimp']:g} · FCmax {d['hrmax_used']}"),
            card("Énergie", f"{e['total_kcal']} <small>kcal</small>", f"repos {e['resting_kcal']} · actif {e['active_kcal']}"),
            card("FC journée", f"{d['hr_mean']:.0f} <small>bpm</small>", f"min {d['hr_min']} · max {d['hr_max']}"),
            card("Couverture", f"{d['coverage_h']:g} <small>h</small>", "données au poignet ce jour"),
        ])
        hourly_steps = {int(h): v["steps"] for h, v in d["hourly"].items()}
        hourly_hr = {int(h): v["hr"] for h, v in d["hourly"].items()}
        warn = ""
        if e["profile_defaults_used"] or d.get("age_default_used"):
            warn = '<p class="caveat">Profil non renseigné : âge 30 ans, 70 kg, 175 cm et sexe masculin utilisés par défaut. Crée <code>lab/profile.json</code> pour des kcal et des zones justes.</p>'
        sections.append(f"""<section><h2>Journée du {d['date']}</h2><div class="grid">{cards}</div>
<p class="small">Minutes par zone (% FCmax) : {zone_txt}</p>
{bars(hourly_steps, 'pas', 'Pas par heure')}{bars(hourly_hr, 'bpm', 'FC moyenne par heure')}{warn}</section>""")

    trend_rows = "".join(
        f"<tr><td>{n['date']}</td><td>{dur(n['asleep_min'])}</td><td>{n['efficiency_pct']:.0f} %</td><td>{n['resting_hr_bpm']:.0f}</td>"
        f"<td>{(n.get('hrv_night') or {}).get('rmssd_ms', '—')}</td><td>{(n.get('respiration') or {}).get('rpm', '—')}</td>"
        f"<td>{n['skin_temp_c']}</td><td>{(n.get('recovery') or {}).get('score') or '—'}</td></tr>" for n in nights)
    sections.append(f"""<section><h2>Tendances</h2><table><thead><tr><th>Nuit</th><th>Sommeil</th><th>Effic.</th><th>FC repos</th>
<th>RMSSD</th><th>Resp.</th><th>Peau °C</th><th>Récup.</th></tr></thead><tbody>{trend_rows}</tbody></table>
<p class="small">La récupération et l'écart de température s'activent après 3 nuits de référence.</p></section>""")

    batt = dev["battery_mv"]
    batt_txt = f"{batt[-1][1] / 1000:.2f} V ({hm(batt[-1][0])})" if batt else "—"
    opt = dev["optical_windows"]
    ev = dev["event_counts"]
    unused = dev["unused_streams"]
    sections.append(f"""<section><h2>Capteurs et qualité des données</h2><div class="grid">
{card('Couverture 1 Hz', f"{dev['coverage_pct']} %", f"{dev['records']:,} secondes · {hm(dev['first'])} → {hm(dev['last'])}".replace(',', ' '))}
{card('Batterie', batt_txt, 'tension rapportée par l’événement BATTERY_LEVEL')}
{card('Fenêtres optiques R26', str(opt['n']), f"acceptées {opt['accepted_pct']} % · amplitude médiane {opt['median_amplitude']:.0f}")}
{card('Événements', str(sum(ev.values())), ' · '.join(f'{k.lower()} {v}' for k, v in sorted(ev.items(), key=lambda kv: -kv[1])[:4]))}
</div>
<table><thead><tr><th>Flux capteur</th><th>Utilisation</th></tr></thead><tbody>
<tr><td>FC 1 Hz (R18)</td><td>FC repos, FC journée, zones, charge, énergie, stades</td></tr>
<tr><td>Intervalles R-R (R18 + live)</td><td>RMSSD/SDNN/pNN50 sur battements successifs, respiration (ASR), VFC locale des stades</td></tr>
<tr><td>Gravité 3 axes + accélération dynamique</td><td>Actigraphie Cole-Kripke, mouvement par heure, réveils</td></tr>
<tr><td>Compteur de pas, classe d'activité</td><td>Pas, minutes de marche/course</td></tr>
<tr><td>Température cutanée + 2 canaux auxiliaires</td><td>Température de sommeil, écart à la référence</td></tr>
<tr><td>État du bracelet (éveil/immobile/sommeil)</td><td>Détection de la période de sommeil</td></tr>
<tr><td>Fenêtres optiques R26</td><td>Indice de qualité du signal (acceptation, amplitude)</td></tr>
<tr><td>Optique brute R20 / IMU 100 Hz R21</td><td>{'reçus : ' + str(unused['r20_optical']) + ' / ' + str(unused['r21_imu']) if any(unused.values()) else 'non envoyés par le bracelet en mode historique normal (décodeurs prêts)'}</td></tr>
<tr><td>SpO₂</td><td>aucun champ décodé fiable dans les trames : non calculée</td></tr>
</tbody></table></section>""")

    sections.append("""<section><h2>Méthodes</h2><ul class="small">
<li>Décodage : contrat NOOP WHOOP 5/MG (R18, R26, R20, R21, paquets 40/48/49), CRC16 en-tête et CRC32 corps vérifiés.</li>
<li>Sommeil : état du bracelet + Cole-Kripke (poids te Lindert 30 s) sur la gravité au-dessus d'un plancher de bruit de 10 mg.</li>
<li>VFC : RMSSD sur battements réellement successifs (secondes consécutives, 300–2000 ms, saut ≤ 20 %), nuit entière et dernier tiers.</li>
<li>Respiration : arythmie sinusale respiratoire, pic 0,13–0,45 Hz (Welch) sur fenêtres de 2 min couvertes à ≥ 80 %.</li>
<li>Charge : TRIMP d'Edwards sur zones de %FCmax (Tanaka), ramené sur 0–21. Énergie : Keytel 2005 + Mifflin-St Jeor.</li>
<li>Récupération : z-scores de ln(RMSSD) et de la FC au repos par rapport à ta moyenne (≥ 3 nuits).</li>
</ul></section>""")

    generated = dt.datetime.fromtimestamp(result["generated"]).strftime("%d/%m/%Y %H:%M")
    return f"""<!doctype html><html lang="fr"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>WHOOP Lab</title><style>
:root{{--bg:#f7f7f5;--fg:#1d1d1f;--muted:#6e6e73;--card:#fff;--line:#e3e3e0;--accent:#2f6fde;
--wake:#e0605a;--rem:#9b7be8;--light:#7fb2f0;--deep:#2a4fa8}}
@media (prefers-color-scheme:dark){{:root:not([data-theme="light"]){{--bg:#151517;--fg:#f2f2f2;--muted:#a1a1a6;--card:#1f1f22;--line:#333338;--accent:#6f9cf0}}}}
:root[data-theme="dark"]{{--bg:#151517;--fg:#f2f2f2;--muted:#a1a1a6;--card:#1f1f22;--line:#333338;--accent:#6f9cf0}}
body{{margin:0;background:var(--bg);color:var(--fg);font:15px/1.5 -apple-system,system-ui,sans-serif}}
main{{max-width:960px;margin:0 auto;padding:24px 16px 48px}}h1{{font-size:24px;margin:0 0 4px}}h2{{font-size:18px;margin:32px 0 12px}}
.sub,.small,.note,figcaption{{color:var(--muted);font-size:13px}}.grid{{display:grid;grid-template-columns:repeat(auto-fill,minmax(170px,1fr));gap:10px}}
.card{{background:var(--card);border:1px solid var(--line);border-radius:12px;padding:12px}}.card.muted .value{{color:var(--muted)}}
.label{{font-size:12px;color:var(--muted);text-transform:uppercase;letter-spacing:.03em}}.value{{font-size:24px;font-weight:600;margin:2px 0}}
.value small{{font-size:13px;font-weight:400;color:var(--muted)}}figure{{margin:16px 0}}.chart{{width:100%;height:auto}}
.chart text{{fill:var(--muted);font-size:10px}}.dot{{display:inline-block;width:9px;height:9px;border-radius:2px;margin:0 4px 0 10px}}
table{{width:100%;border-collapse:collapse;font-size:13px;background:var(--card);border-radius:12px;overflow:hidden}}
th,td{{text-align:left;padding:8px 10px;border-bottom:1px solid var(--line)}}.caveat{{font-size:13px;color:var(--muted);border-left:3px solid var(--line);padding-left:10px}}
.tips{{padding-left:18px}}code{{font-size:12px}}
</style></head><body><main><h1>WHOOP Lab</h1><div class="sub">WHOOP 5.0 · généré le {generated} · données locales, algorithmes ouverts</div>
{''.join(sections)}</main></body></html>"""


def main() -> int:
    subprocess.run([sys.executable, str(HERE / "ingest.py")], check=True, capture_output=True)
    result = analyze.run()
    out = HERE / "report.html"
    out.write_text(render(result), encoding="utf-8")
    print(out)
    return 0


if __name__ == "__main__":
    sys.exit(main())

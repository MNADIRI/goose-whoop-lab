> **Review v10:** certaines descriptions ci-dessous documentent v9. Les changements de statut,
> les époques inconnues et le retrait du faux CPC/ODI sont détaillés dans
> [OPUS_REVIEW.md](../docs/review/OPUS_REVIEW.md). Le Python historique n'est pas encore un oracle de parité v10.

# WHOOP 5.0 : des capteurs à l'écran

Source unique : toutes les valeurs physiologiques de l'app sont calculées **sur l'iPhone** par le cœur
Rust (`Rust/core/src/whoop_lab_analysis.rs`, méthode de pont `whoop_lab.analyze`). C'est un portage du
labo Python de référence (`lab/analyze.py`), qui sert à valider le Rust et à produire le rapport HTML.

Chaîne : bracelet → BLE → trames vérifiées (CRC) dans `decoded_frames` → table 1 Hz `whoop_lab_biometric`
(incrémentale) → analyse par jour local, du premier jour de données à aujourd'hui, mise en cache dans
`whoop_lab_days` (un jour n'est recalculé que si ses données changent) → `WhoopLabResultsStore` → écrans.

Écrans : **Aujourd'hui** (Home) = un jour à la fois, avec sélecteur de jour ; **Tendances** (Health) =
historique de chaque mesure ; écrans de détail = même vue ; **Diagnostic** = rapports internes goose.

## 1. Données brutes extraites

| Trame | Champ brut (offset) | Unité après décodage | Fréquence |
|---|---|---|---|
| R18 (type 47, 124 o) | FC (22) | bpm | 1 Hz |
| | Intervalles R-R (23 nombre, 24+2i) | ms = (ticks×1000+512)//1024 | 0–4 par seconde (~75 % des battements la nuit) |
| | Gravité x/y/z (45, 49, 53, f32) | g | 1 Hz |
| | Accélération dynamique (41, f32) | g | 1 Hz |
| | Compteur de pas (57, ou 61 si surcharge logicielle) | pas cumulés | 1 Hz |
| | Classe d'activité (63) | immobile / marche / course | 1 Hz |
| | Température peau (73) | °C (brut/100, accepté 5–45) | 1 Hz |
| | Températures auxiliaires (69, 71) | °C (brut/10) | 1 Hz, conservées, non affichées |
| | État du bracelet (81, bits 4–5) | éveil / immobile / sommeil / levé | 1 Hz |
| | Bits poignet (81, bits 0–1) | brut | 1 Hz |
| | FC alternative, qualité FC, octets optiques, mots d'état (33–39, 75–82, 106–113) | brut, sens non établi | conservés, non interprétés |
| R26 (type 47, 88 o) | Signal optique : base + 24 deltas | codes bruts | fenêtres de 25 échantillons |
| | Acceptation du signal, poignet, mouvement | brut | par fenêtre |
| R20 (2140 o) | Optique brute 5 blocs × 2 voies | codes bruts | non envoyé en synchro normale |
| R21 (1244 o) | IMU 100 Hz accéléro + gyro | g, °/s | non envoyé en synchro normale |
| Paquet 40 | FC + R-R en direct | bpm, ms | en direct quand l'app est connectée |
| Paquet 48 | Événements (batterie, poignet, charge, double tap, BLE) | horodatés, tension batterie en mV | à l'événement |
| Paquet 49 | Début/fin d'historique | marqueurs | à chaque sync |

Non disponible : SpO₂ (aucun champ fiable dans les trames, rien n'est inventé).

## 2. Traitements, méthodes et valeurs produites

| Valeur affichée | Entrées | Méthode | Où dans l'app |
|---|---|---|---|
| **Période de sommeil** (coucher, endormissement, réveil, temps au lit) | État du bracelet, gravité | Plus long bloc « sommeil » (trous ≤ 20 min fusionnés), étendu en arrière sur « immobile » pour le coucher | Aujourd'hui › Nuit |
| **Sommeil / éveil par époque de 30 s** | Gravité | Cole-Kripke (poids te Lindert 30 s) sur la variation de gravité au-dessus d'un plancher de bruit de 10 mg ; échelle /1000 pour que le retournement isolé reste du sommeil | Hypnogramme |
| **Stades** (léger, profond, paradoxal, éveil) | FC, R-R, mouvement | Règles transparentes : profond = FC basse + VFC locale haute + immobile, 1re moitié ; paradoxal = variabilité de FC haute, immobile, après 60 min ; sinon léger ; époque isolée lissée. Approximatif, sans EEG | Aujourd'hui › Nuit, hypnogramme |
| **Efficacité, latence, réveils** | Stades, temps au lit | sommeil / temps au lit ; endormissement − coucher ; passages vers l'éveil | Aujourd'hui › Nuit |
| **Score Sommeil (%)** | Durée de sommeil | sommeil / 8 h (plafonné à 100) | Aujourd'hui › scores |
| **FC au repos** | FC pendant le sommeil | Plus basse moyenne glissante de 5 min | Aujourd'hui › Moniteur, Nuit |
| **VFC : RMSSD, SDNN, pNN50, ln RMSSD** | R-R pendant le sommeil | Battements réellement successifs uniquement (secondes consécutives, 300–2000 ms, saut ≤ 20 %, ≥ 100 différences) ; nuit entière et dernier tiers | Aujourd'hui › Moniteur (RMSSD), Nuit |
| **Fréquence respiratoire** | R-R pendant le sommeil | Arythmie sinusale respiratoire : tachogramme 1 Hz, fenêtres de 2 min couvertes ≥ 80 %, pic Welch 0,13–0,45 Hz ≥ 3× la médiane de la bande, médiane des fenêtres | Aujourd'hui › Moniteur, Nuit |
| **Température peau** | Peau (R18) pendant le sommeil | Médiane de la nuit ; écart = nuit − moyenne des nuits précédentes (≥ 3) | Aujourd'hui › Moniteur (écart, ou absolu tant que la référence se construit) |
| **Récupération (0–100)** | ln RMSSD, FC au repos | z-scores vs ta référence (≥ 3 nuits), 0,6×z VFC + 0,4×(−z FC), logistique | Aujourd'hui › scores, Nuit |
| **Pas** | Compteur de pas | Somme des incréments positifs (< 300/s, ignore les remises à zéro) | Aujourd'hui › Journée |
| **Marche / course** | Classe d'activité | Secondes classées marche/course | Aujourd'hui › Journée |
| **Zones FC, activité modérée+** | FC, âge | %FCmax (Tanaka 208−0,7×âge) ; modérée+ = ≥ 40 % réserve cardiaque | Aujourd'hui › Journée |
| **Charge du jour (0–21)** | Zones FC | TRIMP d'Edwards (minutes × numéro de zone), ramené sur 0–21 | Aujourd'hui › scores (Strain, en %), Journée |
| **Charge cardio 7 j / 28 j, ratio A/C** | TRIMP quotidiens | Somme 7 j ; moyenne hebdo sur 28 j ; ACWR (Gabbett) après 21 jours | Aujourd'hui › Cardio Load, Health › Cardio Load |
| **Stress (0–100)** | FC, FC repos, activité, accélération | Éveillé et immobile (activité « immobile », accél. < 0,03 g) : (FC − FC repos) / (40 % réserve) par tranche de 5 min ; bandes repos/bas/moyen/haut | Aujourd'hui › Stress, Health › Stress |
| **Énergie dépensée** | FC, profil | Repos : Mifflin-St Jeor ; actif : Keytel 2005 au-dessus de FC repos + 20 | Aujourd'hui › carte « Énergie dépensée » (remplace Energy Bank), Journée |
| **FC journée, par heure** | FC | moyenne, min, max ; moyenne horaire | Aujourd'hui › Journée (graphique) |
| **Qualité** | Toutes les secondes R18, R26 | Couverture 1 Hz ; % de fenêtres optiques acceptées | Aujourd'hui › Qualité |
| **Batterie** | Événements 48 | Dernière tension rapportée | Rapport Mac |

### Ajouts de l'algorithme 4 (intégration de NOOP, voir `docs/WHOOP5_NOOP_AUDIT.md`)

Toutes ces valeurs sont dans le JSON de `whoop_lab.analyze` (pas encore affichées dans l'app). Chacune porte
un statut et une provenance (`provenance` à la racine).

| Valeur | Entrées | Méthode | Statut |
|---|---|---|---|
| `hrv_qc` | R-R | Rejets par plage (300–2000 ms), Malik 20 % et médiane locale ±2 (20 %), comptés séparément ; part d'artefacts et note de qualité | Expérimental |
| `hrv_rolling_5min` | R-R nettoyés | RMSSD par tranche de 5 min (≥ 30 différences), médiane, p10, p90 | Expérimental |
| `baselines` | Nuits précédentes | EWMA winsorisée (NOOP) : demi-vie 14 nuits ; calibrage < 4 nuits, provisoire, fiable ≥ 14, périmée | Expérimental |
| `recovery` | RMSSD, FC repos, respiration, efficacité | Portage de NOOP : poids 0,60 / 0,20 / 0,05 / 0,15, renormalisés si un terme manque, logistique (Z = 0 → 58 %), chaque composante exposée | Expérimental |
| `skin_temp` | Peau (R18) | Écart-type, pente °C/h, médiane horaire, écart à la référence ; température de peau, pas centrale | Décodé, non validé |
| `sleep_detail` | Stades, gravité, FC | Minutes de sommeil selon le firmware et selon l'inférence locale, WASO, réveils par heure, % de mouvement, FC nocturne | Inféré |
| `sleep_regularity` | 7 dernières nuits | Écart-type des heures d'endormissement et de réveil | Expérimental |
| `optical` | R26 | Rafales, fréquence estimée (~25 Hz), filtre passe-bande 0,5–4 Hz, pouls par autocorrélation et pics, IBI, amplitude, temps de montée, SNR ; « validé » si l'écart à la FC R18 est ≤ 5 bpm | Validé contre la FC (pouls) ; amplitude expérimentale |
| `training` | FC, accélération dynamique | Karvonen (%RFC), TRIMP d'Edwards sur %RFC et de Banister, échelle NOOP 0–21, détection d'entraînement, récupération de FC (60 et 120 s), dérive cardiaque | Expérimental |
| `load.foster` | TRIMP quotidiens | Monotonie = moyenne / écart-type sur 7 jours, contrainte = charge hebdomadaire × monotonie | Expérimental |

Couche décodée (tables `whoop5_*`) et export : `whoop5.ingest`, `whoop5.inventory`, `whoop5.export` (JSONL, CSV,
binaire brut et `schema.json` avec unités, statuts et provenance).

### Ajouts des algorithmes 6 et 7 (champs du bracelet rétro-ingénierés, voir docs/OPTICAL.md)

| Valeur | Entrée brute | Méthode | Où | Statut |
|---|---|---|---|---|
| **SpO₂ (bracelet)** | R18 octet 82 | Fenêtres de 30 s toutes les 20 min en sommeil ; médiane par fenêtre des valeurs 70–100, codes d'état écartés (au moins 10 s valides par fenêtre) ; médiane, minimum, fenêtres < 95 / < 90 % | Aujourd'hui › Corps | Décodé, non validé |
| **Masque de qualité VFC** | R18 `cardiac_flags` bits 0, 4, 6 | R-R ignorés dans les secondes marquées « mouvement » ; RMSSD sans masque conservé pour comparaison | VFC partout | Expérimental. Nuit du 9–10 : artefacts 1,3 → 0,5 %, RMSSD 55,0 → 54,4 ms |
| **FC de repos du bracelet** | R18 `rr_adjacent` octet haut | médiane et 10e centile pendant le sommeil, écart à la nôtre | Corps (détail FC au repos) | Décodé, non validé (p10 44 contre 43,3 pour notre calcul) |
| **Perfusion** | R18 amplitude optique | médiane sur les secondes immobiles du sommeil, écart aux nuits précédentes (≥ 3) | Corps | Expérimental |
| **Température de la carte** | R18 temp. auxiliaire 2 | médiane nocturne, écart peau − carte | résultats (`environment_temp`) | Décodé, non validé |
| **Cadence de marche** | R18 période du pas | 60 × k / valeur brute, k auto-étalonné sur le compteur de pas (≥ 5 min de marche ; 196,6 mesuré, 200 par défaut) | Aujourd'hui › Activité | Expérimental |

### Algorithme 8 : indicateurs avancés (`Rust/core/src/health_indices.rs`)

| Indicateur | Entrées | Méthode (référence) | Résultat 9–11 nov. |
|---|---|---|---|
| **VFC spectrale** | R-R (tachogramme 1 Hz) | Bandes Task Force 1996 : VLF 0,0033–0,04, LF 0,04–0,15, HF 0,15–0,4 Hz ; fenêtres de 5 min ; par phase | nuit 1 : LF 979, HF 600 ms², LF/HF 1,53 ; nuit 2 : LF/HF 2,38 |
| **Couplage cardio-respiratoire** | R-R | Version R-R seule de la CPC (Thomas 2005), fenêtres de 512 s : stable (HF/(LF+VLF) ≥ 0,6 et pic ≥ 20 %), instable (LF/HF ≥ 2), VLF (≥ 50 %) ; seuils calés sur tes nuits | stable 20,6 % / 0 % |
| **Phases v2** | phases à règles + CPC | stable en début de nuit → profond ; instable/VLF → pas profond ; VLF après 1 h → paradoxal. Les comptes v1 restent dans `stages_rule_v1_min` | nuit 1 : profond 29 → 59 min, paradoxal 98 → 111 |
| **Régularité respiratoire** | R-R | fréquence par fenêtre de 2 min (RSA) : CV, variation moyenne, % de fenêtres irrégulières (> 3/min de la médiane) | CV 11,4 % / 15,8 % |
| **Perturbations respiratoires** (0–100) | SpO₂ du bracelet, FC, respiration | ODI-3 sur les fenêtres SpO₂ + accélérations de FC ≥ 8 bpm (cf. Azarbarzin 2014) + irrégularité ; indicateur de dépistage, pas un IAH | 37 / 61 |
| **Mouvement** | gravité, accélération dynamique, indicateurs du bracelet | secondes en mouvement, épisodes/h, changements de position (≥ 30° entre minutes) | 3,2 / 4,0 changements/h |
| **Sédentarité** | pas, accélération | minutes inactives (< 10 pas, < 0,15 g), périodes ≥ 30 min | 10 périodes, 609 min le 10 |
| **VFC au réveil** | R-R des 10 dernières min de sommeil | RMSSD sans les secondes en mouvement | 53,5 / 62,8 ms |
| **Peau à l'endormissement** | température de peau | moyenne 20–40 min après l'endormissement − 15 min avant le coucher (vasodilatation distale, Kräuchi 1999) | −1,2 / +0,5 °C |
| **Rythme circadien** | 7 jours | cosinor 24 h de la FC (creux) et de la peau (pic) ; indice de régularité du sommeil (SRI, Phillips 2017) | creux de FC 5 h 00, pic de peau 8 h, SRI 70,8 |
| **Type de séance** | pas/min, cadence, accélération, %RFC | course / marche rapide / marche / vélo-rameur / renforcement / autre effort | |
| **VO₂max** | FC max, FC de repos sur 7 nuits | Uth 2004 : 15,3 × FCmax / FCrepos (± ~10 %) ; FCmax = max(Tanaka, observée) | ~65–66 (FC de repos basse : la formule surestime probablement) |
| **Fatigue** (0–100) | VFC 7 j, FC de repos 7 j, charge, sommeil | ln RMSSD 7 j vs référence 30 j en unités de 0,5 SD (Plews & Buchheit) ×0,35 ; FC de repos ×0,20 ; ATL/CTL de Banister ×0,25 ; dette 7 j ×0,20 ; logistique ; CV du ln RMSSD 7 j exposé | après ≥ 5 nuits de référence |
| **Récupération v2** (0–100) | nuit, réveil, séances, charge | z VFC nuit ×0,30 + VFC réveil ×0,10, FC repos ×0,20, respiration ×0,05, performance de sommeil ×0,15, **récupération cardiaque après effort** ×0,10, **forme de Banister (TSB/CTL)** ×0,10 ; logistique (Z = 0 → 58) | après 3 nuits |
| **Alerte précoce** | nuit vs ≤ 30 nuits | peau ≥ +0,5 °C, FC repos ≥ +3 bpm, ln RMSSD ≤ −1,5 SD, respiration ≥ +1/min, SpO₂ ≤ −2 % ; « à surveiller » ≥ 2 signaux, « alerte » ≥ 3 ou peau + FC (cf. Mishra 2020, Smarr 2020) ; surentraînement suspecté si fatigue ≥ 65 et VFC basse 3 nuits | après 5 nuits |

**Modèles externes étudiés** :
- **HAR** : HARNet (ssl-wearables, Oxford, poids ouverts) est le meilleur modèle pré-entraîné au poignet, mais il lui faut
  l'accéléromètre brut en continu (30 Hz). On ne l'a qu'en direct, sur commande. Il pourrait servir pendant une séance
  avec IMU allumée, une fois converti pour l'iPhone ; sur l'historique à 1 Hz, il n'est pas applicable, d'où la
  classification par règles.
- **Sommeil** : WatchSleepNet (2025) est ouvert, mais ses entrées doivent être vérifiées et il faudrait le porter (PyTorch
  vers CoreML ou Rust). Le gain est à valider contre une référence.

## 3. Cohérence et redondances traitées

- **Une seule valeur par mesure.** Les cartes goose (Sleep, Recovery, Strain, Stress, Cardio Load, FC repos,
  VFC, respiration, température, SpO₂) affichent les valeurs du labo ; leurs écrans de détail montrent la
  même vue que Home.
- **Energy Bank supprimé** au profit de l'énergie dépensée (kcal) : c'était un indice propre à goose sans
  méthode publiée, redondant avec Récupération + Charge.
- **Strain** : une seule mesure (TRIMP d'Edwards). Sur Home elle apparaît en % de 21, dans le détail en /21.
- **VFC** : RMSSD nuit entière sur Home ; le dernier tiers et SDNN sont des compléments dans le détail.
- **FC** : 1 Hz de l'historique pour tout ; la FC « en direct » de l'app reste un affichage temps réel,
  pas une métrique calculée.
- **Diagnostic** : les écrans Packet Inputs, Algorithms, Reference Comparisons et Calibration gardent les
  rapports internes de goose ; ils servent au débogage, pas aux métriques.

## 4. Limites

- Stades : 65–73 % d'accord par époque au mieux pour ce type de méthode ; le profond est le moins fiable.
- Respiration : estimée de la VFC, pas mesurée directement.
- Énergie et zones : profil par défaut (30 ans, 70 kg, 175 cm, homme) tant que `lab/profile.json` est absent.
- Récupération et écart de température : après 3 nuits ; ratio A/C après 21 jours.
- Calories : actives (Keytel) affichées en premier ; la dépense totale ajoute le métabolisme de base, proratisé sur la partie écoulée et portée de la journée.
- Pas : compteur du bracelet au poignet ; Apple Health compte le téléphone, les totaux diffèrent.

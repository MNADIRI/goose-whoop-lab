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

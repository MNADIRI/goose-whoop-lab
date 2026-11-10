# WHOOP 5.0 : des capteurs à l'écran

Source unique : toutes les valeurs physiologiques affichées dans l'app (Home, Health, écrans de détail)
viennent du labo Mac (`lab/analyze.py` → `iCloud Drive/whoop-lab/results/latest.json`). L'app ne garde
ses propres calculs que tant que le labo n'a pas encore de résultat, et pour les écrans de diagnostic
(Packet Inputs, Algorithms, Reference Comparisons, Calibration).

Chaîne : bracelet → BLE → goose (iPhone, trames brutes en base) → export iCloud toutes les 10 min →
`ingest.py` (décodage, tables typées) → `analyze.py` → `report.py` (rapport HTML + `latest.json`) → app.

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
| **Période de sommeil** (coucher, endormissement, réveil, temps au lit) | État du bracelet, gravité | Plus long bloc « sommeil » (trous ≤ 20 min fusionnés), étendu en arrière sur « immobile » pour le coucher | Home › Nuit, Health › Sleep |
| **Sommeil / éveil par époque de 30 s** | Gravité | Cole-Kripke (poids te Lindert 30 s) sur la variation de gravité au-dessus d'un plancher de bruit de 10 mg ; échelle /1000 pour que le retournement isolé reste du sommeil | Hypnogramme |
| **Stades** (léger, profond, paradoxal, éveil) | FC, R-R, mouvement | Règles transparentes : profond = FC basse + VFC locale haute + immobile, 1re moitié ; paradoxal = variabilité de FC haute, immobile, après 60 min ; sinon léger ; époque isolée lissée. Approximatif, sans EEG | Home › Nuit, hypnogramme |
| **Efficacité, latence, réveils** | Stades, temps au lit | sommeil / temps au lit ; endormissement − coucher ; passages vers l'éveil | Home › Nuit |
| **Score Sommeil (%)** | Durée de sommeil | sommeil / 8 h (plafonné à 100) | Home › scores |
| **FC au repos** | FC pendant le sommeil | Plus basse moyenne glissante de 5 min | Home › Moniteur, Nuit |
| **VFC : RMSSD, SDNN, pNN50, ln RMSSD** | R-R pendant le sommeil | Battements réellement successifs uniquement (secondes consécutives, 300–2000 ms, saut ≤ 20 %, ≥ 100 différences) ; nuit entière et dernier tiers | Home › Moniteur (RMSSD), Nuit |
| **Fréquence respiratoire** | R-R pendant le sommeil | Arythmie sinusale respiratoire : tachogramme 1 Hz, fenêtres de 2 min couvertes ≥ 80 %, pic Welch 0,13–0,45 Hz ≥ 3× la médiane de la bande, médiane des fenêtres | Home › Moniteur, Nuit |
| **Température peau** | Peau (R18) pendant le sommeil | Médiane de la nuit ; écart = nuit − moyenne des nuits précédentes (≥ 3) | Home › Moniteur (écart, ou absolu tant que la référence se construit) |
| **Récupération (0–100)** | ln RMSSD, FC au repos | z-scores vs ta référence (≥ 3 nuits), 0,6×z VFC + 0,4×(−z FC), logistique | Home › scores, Nuit |
| **Pas** | Compteur de pas | Somme des incréments positifs (< 300/s, ignore les remises à zéro) | Home › Journée |
| **Marche / course** | Classe d'activité | Secondes classées marche/course | Home › Journée |
| **Zones FC, activité modérée+** | FC, âge | %FCmax (Tanaka 208−0,7×âge) ; modérée+ = ≥ 40 % réserve cardiaque | Home › Journée |
| **Charge du jour (0–21)** | Zones FC | TRIMP d'Edwards (minutes × numéro de zone), ramené sur 0–21 | Home › scores (Strain, en %), Journée |
| **Charge cardio 7 j / 28 j, ratio A/C** | TRIMP quotidiens | Somme 7 j ; moyenne hebdo sur 28 j ; ACWR (Gabbett) après 21 jours | Home › Cardio Load, Health › Cardio Load |
| **Stress (0–100)** | FC, FC repos, activité, accélération | Éveillé et immobile (activité « immobile », accél. < 0,03 g) : (FC − FC repos) / (40 % réserve) par tranche de 5 min ; bandes repos/bas/moyen/haut | Home › Stress, Health › Stress |
| **Énergie dépensée** | FC, profil | Repos : Mifflin-St Jeor ; actif : Keytel 2005 au-dessus de FC repos + 20 | Home › carte « Énergie dépensée » (remplace Energy Bank), Journée |
| **FC journée, par heure** | FC | moyenne, min, max ; moyenne horaire | Home › Journée (graphique) |
| **Qualité** | Toutes les secondes R18, R26 | Couverture 1 Hz ; % de fenêtres optiques acceptées | Home › Qualité |
| **Batterie** | Événements 48 | Dernière tension rapportée | Rapport Mac |

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
- Les valeurs se mettent à jour quand `report.py` tourne sur le Mac.

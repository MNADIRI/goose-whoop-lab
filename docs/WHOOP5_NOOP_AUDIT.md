# Audit NOOP → goose (WHOOP 5.0)

NOOP audité au commit `7be90e0` (7 juin 2026), code source lu (pas seulement le README) : `Packages/WhoopProtocol`
(schéma JSON, PostHooks, HistoricalStreams, DeviceFamily, Framing), `StrandAnalytics` (HRVAnalyzer, RecoveryScorer,
StrainScorer, Baselines, HRZones, SleepStager, WorkoutDetector), `WhoopStore` (migrations v1–v9) et leurs tests.

## 1. Constat principal

NOOP est d'abord une app **WHOOP 4.0** (enveloppe CRC8, historique type 47 « V24/V12 », flux brut type 43 de 1917 /
1921 octets). Son chemin **WHOOP 5.0 est porté depuis goose** (README et ATTRIBUTION de NOOP : UUID `fd4b0001-…`,
CRC16-Modbus, CLIENT_HELLO, types « puffin »). NOOP ne contient **aucun décodeur R18, R20, R21 ou R26** : ces
enregistrements n'existent que sur la 5.0 et goose était déjà en avance sur le protocole.

Ce qui avait de la valeur dans NOOP, et qui a été repris :
- les **algorithmes** : référence personnelle EWMA winsorisée, récupération composite, TRIMP Banister, Karvonen,
  détection d'entraînement, rejet des ectopiques par médiane locale ;
- les **tables de noms** d'événements et de commandes, plus complètes ;
- l'échelle IMU **vérifiée sur 4.0** (accéléromètre 1/4096 g, gyroscope 2000/32768 °/s), reprise comme valeur provisoire à
  re-vérifier sur la 5.0 ;
- la séparation brut / décodé (`RawOutbox` / `StreamStore`), que goose avait déjà (`raw_evidence` / `decoded_frames`).

Ce qui n'a **pas** été repris, et pourquoi :
- les offsets V24 (spo2_red, resp_rate_raw, etc.) : ils viennent d'une autre trame (4.0), et les appliquer à la 5.0
  inventerait des champs ;
- la mise en page de l'événement batterie 4.0 (soc@17, mV@21) : elle ne correspond pas aux trames 5.0 ;
- l'UI et le coach IA de NOOP, hors périmètre.

## 2. Comparaison d'architecture

| Couche | NOOP | goose (avant) | goose (après) |
|---|---|---|---|
| BLE | Swift CoreBluetooth, `Reassembler` | Swift CoreBluetooth, réassembleur maison | idem + **contrôle CRC16 de l'en-tête avant de croire la longueur** (Swift ×2 et Rust) |
| Décodage | schéma JSON + PostHooks (4.0) | `protocol.rs` (enveloppe) + R18 partiel + Python `lab/whoop5` | **`whoop5_records.rs`** : R18 complet, R20, R21, R26, paquets 40/48/49/50, catalogue de champs |
| Stockage brut | `RawOutbox` | `raw_evidence` + `decoded_frames` (immuables) | inchangé (couche A) |
| Stockage décodé | `StreamStore` (hr, rr, spo2…) | `whoop_lab_biometric` (1 Hz, 11 colonnes) | **`whoop5_*`** : une table par famille, BLOBs pour les tableaux (couche B) |
| Analyse | `StrandAnalytics` (Swift) | `whoop_lab_analysis.rs` | + `hrv_qc`, `optical`, `imu`, `baselines`, `training`, `dsp` |
| Export | CSV | JSON | **JSONL, CSV (format long), binaire brut, `schema.json`** |

## 3. Écarts fonctionnalité par fonctionnalité

| Fonctionnalité | NOOP | goose avant | État |
|---|---|---|---|
| UUID, CLIENT_HELLO, CRC16 / CRC32 | porté de goose | oui | déjà présent |
| Faux 0xAA dans les données | `firstIndex(of: 0xAA)` | longueur crue sans contrôle | **corrigé** (CRC16 de l'en-tête vérifié, sinon avance d'un octet) |
| Trames multiples ou partielles | oui | oui | déjà présent, testé |
| Doublons | non | collision d'ID corrigée | **dédoublonnage (ts, record_index)** en couche B, comptés |
| Numéros manquants | non | non | **trous de `record_index`** dans l'inventaire |
| Couches inconnues | région « unknown » | ignorées | **`whoop5_unknown`** (type, couche, taille) |
| R18, tous les champs | — | 7 champs | **33 champs nommés + octets non interprétés conservés** |
| R20 optique brut | — | Python seulement | **Rust** : 5 blocs × 2 voies × ≤ 50, rembourrage contrôlé |
| R21 IMU | (4.0 type 43) | Python seulement | **Rust** : int16 bruts conservés + conversions provisoires |
| R26 optique compact | — | Python seulement | **Rust** : base + 24 deltas → 25 échantillons (i64, sans débordement) |
| Événements | table 4.0 | table partielle | **table fusionnée**, nom `EVENT_n` si inconnu, données brutes gardées |
| VFC avec contrôle qualité | médiane ±2 voisins, 20 % | saut 20 % | **plage + Malik + médiane, statistiques de rejet** |
| VFC glissante | non | non | **RMSSD par 5 min** |
| Référence personnelle | EWMA winsorisée + statuts | moyenne de 3 nuits | **portée** (calibrage / provisoire / fiable / périmée) |
| Récupération | composite 4 termes | 2 termes | **portée, chaque composante exposée** |
| Karvonen / Banister / Foster | Karvonen, Banister | Edwards sur %FCmax | **ajoutés** (Edwards %RFC, Banister, Foster, ACWR conservé) |
| Détection d'entraînement | WorkoutDetector | non | **portée** + récupération de FC, dérive cardiaque |
| PPG | moyenne de la trame 4.0 | non | **traitement R26 validé contre la FC** |
| IMU six axes | moyennes par axe | non | **module complet**, en attente de vraies trames R21 |
| Température | brute (4.0) | médiane + écart | + **variabilité, tendance, profil horaire, référence** |
| Régularité du sommeil, WASO, fragmentation | partiel | non | **ajoutés** |

## 4. Inventaire des trames WHOOP 5.0 (base réelle de l'iPhone, firmware 50.38.1)

| Paquet / couche | Taille | Trames | Décodeur | Statut |
|---|---|---|---|---|
| 47 / R18 | 124 o | 51 804 (51 744 uniques, 486 indices manquants en 134 trous) | `decode_r18` | FC, R-R, gravité **validés** ; reste décodé, non validé |
| 47 / R26 | 88 o | 1 760 (1 746 uniques) | `decode_r26` | **PPG validé** (voir § 5) |
| 47 / R20 | 2 140 o | 0 | `decode_r20` | testé sur trames synthétiques uniquement |
| 47 / R21 | 1 244 o | 0 | `decode_r21` | testé sur trames synthétiques uniquement |
| 40 FC en direct | 32 o | 2 223 | `decode_live_hr` | validé |
| 48 événements | 24–36 o | 756 | `decode_event` | noms validés ; tension batterie candidate 4 357 mV (plausible à pleine charge) |
| 49 métadonnées | 36 o | 981 | `decode_metadata` | début / fin / historique complet |
| 50 console | 76 o | 1 292 | `decode_console` | texte |
| 36 réponses de commande | 20 o | 1 098 | `protocol.rs` (existant) | — |

Aucune trame n'avait de CRC invalide dans cette base, et aucune couche inconnue n'est apparue.

Offsets, tailles, signe, échelle, unité, conditions de validité et confiance de chaque champ :
`whoop5.field_catalog` (méthode du pont) ou `FIELD_CATALOG` dans `Rust/core/src/whoop5_records.rs`. Tous les
champs sont en little-endian ; les offsets sont donnés dans la trame (= offset dans la charge utile + 8).

## 5. Résultats sur données réelles (nuit du 9 au 10 novembre)

- **R26 = un canal PPG pulsatile à environ 25 Hz** : une fenêtre de 25 échantillons par seconde, continue d'une
  fenêtre à l'autre (rapport de saut 0,8–1,1), par rafales de 40 s toutes les 20 min environ.
  - 26 rafales sur 28 sont validées contre la FC R18 des mêmes secondes, avec une erreur moyenne de **0,6 bpm**.
  - Le rapport signal/bruit spectral vaut 0,92 à 0,96.
  - La longueur d'onde reste inconnue : le canal est donc dit « optique non identifié ». Rien n'est déduit en SpO₂,
    pression artérielle ou indice de perfusion.
- **VFC** : RMSSD 55,0 ms (55,9 avec l'ancien filtre), 7 201 différences successives.
  - 1,3 % d'artefacts rejetés (plage, Malik, médiane).
  - 60 % des battements tombent dans des séquences de moins de 5 battements : c'est la nature du flux R18, pas du bruit.
  - Sur les fenêtres de 5 min : médiane 54,3 ms (p10 46,4, p90 69,0).
- **Sommeil** : 462 min endormi ; le firmware classe 499 min en « sommeil ».
  - Les éveils intra-sommeil (WASO) totalisent 38 min, soit 3,25 réveils par heure.
  - Il y a du mouvement dans 4,4 % du temps.
- **Récupération** : non calculée, faute de référence (0 nuit sur 4 requises). C'est voulu : aucune valeur inventée.
- **Temps de calcul** : 0,62 s au premier passage (Mac, version release), dont l'ingestion de 58 870 trames ;
  0,13 s ensuite grâce au cache.

## 6. Tables ajoutées (migration additive, aucune suppression)

`whoop5_r18`, `whoop5_r26`, `whoop5_r20_slot`, `whoop5_r21`, `whoop5_live_hr`, `whoop5_events`, `whoop5_metadata`,
`whoop5_console`, `whoop5_unknown`, `whoop5_state` (curseur et version du décodeur).
- Chaque ligne garde son `frame_id`, qui remonte à `decoded_frames` puis à `raw_evidence` (octets d'origine), et
  la version du décodeur (`decoder_version`).
- `whoop5.rebuild` régénère la couche B après un changement de décodeur sans toucher la couche A.

## 7. Méthodes ajoutées au pont

| Méthode | Rôle |
|---|---|
| `whoop5.field_catalog` | catalogue des champs |
| `whoop5.ingest` | décodage incrémental vers la couche B |
| `whoop5.inventory` | couverture, doublons, CRC, trous, couches inconnues |
| `whoop5.rebuild` | régénère la couche B |
| `whoop5.export` | `{database_path, output_dir, format: "jsonl"\|"csv", start_ts, end_ts, include_raw}` → fichiers + `schema.json` (unités, statuts, provenance) + `raw_frames.bin` (`[u32 LE longueur][octets]`) |

`whoop_lab.analyze` appelle `whoop5.ingest` à chaque passage. Le résultat contient `provenance` (source, méthode,
unité, statut et version de chaque valeur), et chaque nuit contient `hrv_qc`, `hrv_rolling_5min`, `baselines`,
`skin_temp`, `sleep_detail` et `optical`. Chaque journée contient `training` (Karvonen, Banister, entraînements).

Statuts utilisés : `VALIDATED`, `DECODED_UNVALIDATED`, `EXPERIMENTAL`, `INFERRED`, `IMPORTED`, `UNAVAILABLE`,
`INVALID`. Une valeur manquante vaut `null`, jamais 0.

Parquet n'est pas généré : il faudrait embarquer Arrow dans la bibliothèque iOS. Les CSV et JSONL se convertissent
en une ligne avec pandas ou pyarrow.

## 8. Ce qui est testé et ce qui attend de vraies captures

Testé :
- **Sur vraies trames 5.0** : enveloppe et CRC, R18, R26 et validation PPG, paquets 40/48/49/50, ingestion
  incrémentale, dédoublonnage, inventaire, VFC avec contrôle qualité, respiration, température, sommeil,
  entraînements, Karvonen et Banister.
- **Sur données synthétiques** (tests unitaires Rust) : R20, R21, faux 0xAA du réassembleur, débordement des deltas
  R26, références personnelles et récupération, Foster, export JSONL/CSV, filtres, pics, autocorrélation,
  extraction d'IMU.

À capturer pour valider :
- **R20 et R21** : il faut activer `TOGGLE_PERSISTENT_R20/R21` (commandes 153/154, déjà dans l'app) puis
  synchroniser. C'est seulement alors qu'on pourra vérifier :
  - la fréquence de l'IMU (≈ 100 Hz annoncés) ;
  - l'échelle de l'accéléromètre, avec la fonction `imu::accel_scale_check` (|a| = 1 g au repos) ;
  - l'échelle du gyroscope, qui demande une rotation contrôlée ;
  - la signification de `source`, `drive`, `detector`, `range` et `offset` dans R20.
- **Le reste** :
  - les champs R18 non interprétés (octets 34–35, 40, 65–68, 83–105, 110–112, `unknown_f32`) ;
  - les événements 110 et 120 ;
  - la récupération, la régularité du sommeil et Foster, qui demandent au moins 4 nuits ou 7 jours ;
  - la tension batterie, qui demande une comparaison à une mesure réelle.

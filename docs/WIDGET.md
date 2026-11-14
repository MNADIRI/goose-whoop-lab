# Widget mn health : écran verrouillé et écran d'accueil

Un widget unique qui change de contenu selon le moment de la journée : **Matin** (récupération), **Journée**
(physiologie), **Soir** (préparation du sommeil), **Nuit** (réveil programmé). Formats : rectangle, cercle et ligne
sur l'écran verrouillé ; petit et moyen sur l'écran d'accueil, avec les cartes « aura » de l'app (police Manrope,
chiffres Doto).

## 1. Architecture

| Composant | Où | Rôle |
|---|---|---|
| `MNHealthMetricRepository` | app, `GooseSwift/MNHealthMetricRepository.swift` | rassemble les entrées des producteurs existants et appelle le moteur ; ne calcule rien |
| `MetricQualityEvaluator` | Rust, `daily_context.rs` (`Metric::at` + règles des constructeurs) | qualité, fraîcheur, libellé « en direct », raison d'absence |
| `DailyContextEngine` | Rust, `decide_context` | contexte du moment, événements observés d'abord, replis horaires, hystérésis |
| `SleepSchedulePlanner` | Rust, `plan_schedule` | coucher et heure de décrocher à partir du réveil réel et du plan |
| `DailyInsightEngine` | Rust, `insight` | une phrase déterministe (règles dans un ordre fixe) |
| `DailyHealthSnapshotStore` | partagé, `GooseSwift/DailyHealthSnapshot.swift` | modèles Codable + fichier JSON dans l'App Group `group.com.marwannadiri.goose` |
| `DailyHealthTimelineProvider` | extension, `GooseWorkoutLiveActivityExtension/MNHealthWidget.swift` | rejoue la timeline précalculée ; aucun calcul |

Le pont expose `daily.snapshot`. Aucun traitement du signal ni aucun score dans l'extension WidgetKit. Les séances en
cours gardent la Live Activity existante (ActivityKit) ; le widget n'en est que le résumé.

Rafraîchissement : à chaque nouvelle analyse, changement du réveil, début ou fin de séance, réveil confirmé, FC
(au plus toutes les 10 min) et toutes les 15 min tant que l'app tourne (le Bluetooth la garde active en arrière-plan).
Les rechargements du widget sont limités à un par quart d'heure, sauf changement de contexte ou de valeur principale.

## 2. Audit : chaque métrique et son producteur

| Métrique | Producteur existant | Qualité attribuée | Disponibilité et limites |
|---|---|---|---|
| Récupération 0–100 | `indices.recovery.score` (health_indices / baselines) | dérivée ; « calibrating » tant que la référence VFC se construit | absente si VFC insuffisante ou nuit incomplète : raison du producteur affichée, jamais de score inventé |
| Sommeil vs besoin | `night.asleep_min`, `night.sleep_score.need_min` | dérivée ; « incomplete » (≥ valeur) si couverture < 90 % | borne inférieure sur nuit incomplète |
| VFC nuit + écart | `night.hrv_night.rmssd_ms`, `night.baselines.hrv_rmssd` (delta, z) | mesurée ; « calibrating » sans référence | absente si R-R insuffisants |
| Pouls de repos + écart | `night.resting_hr_bpm`, `night.baselines.resting_hr` | mesurée | — |
| Dette 7 nuits | `smart_wake.plan` → `components.debt_7d_min` (nuits complètes seulement) | dérivée | affichée si ≥ 1 h |
| Efficacité | `night.efficiency_pct` | dérivée | masquée sur nuit incomplète |
| Respiration | `night.respiration.rpm` + `baselines.respiration.z` | estimée | affichée seulement si \|z\| ≥ 2 et référence établie |
| SpO₂ | `night.spo2.median_pct` | estimée | affichée seulement si < 94 % |
| Température | `night.skin_temp_delta_c` | estimée | affichée seulement si \|écart\| ≥ 0,5 °C |
| FC la plus récente | BLE (`liveHeartRateBPM` + horodatage) ou dernier R18 synchronisé (`whoop_lab_biometric`) | mesurée ; « en direct » ≤ 2 min, puis « à HH:MM », périmée après 15 min | jamais « en direct » sur une valeur ancienne |
| Effort % | `day.strain_pct` = 100 × (1 − e^(−TRIMP/250)), comme l'app (repli : `strain_0_21` ramené en %) + moyenne des 14 jours précédents | dérivée ; affichée avec son heure après 2 h | cumul de la journée synchronisée |
| Stress récent 0–100 | `day.stress.last` (dernier bloc de 5 min au repos immobile, ajouté pour ce widget), sinon `day.stress.mean` | estimée | calculé **hors effort** (périodes immobiles seulement) : l'activation cardiaque de l'exercice n'est pas comptée |
| Stress élevé hors effort | `day.stress.minutes.haut` | estimée | affiché si ≥ 30 min |
| Pas | `day.steps` (compteur R18) | estimée | décodage non validé sur ce bracelet |
| Activité en cours | `SportController.current` (type, début, étiquetée ou détectée) | mesurée si étiquetée, estimée sinon | — |
| Dernière séance | `day.training.workouts.sessions` | dérivée | — |
| Réveil, fenêtre | configuration Smart Wake de l'app | lue telle quelle | **jamais modifiée** ; « réveil non programmé » si désarmé |
| Besoin, décrocher, coucher | `smart_wake.plan` (besoin, latence, part de dette, coucher habituel) | estimée | coucher limité par la régularité (§ 4) |

Chaque métrique porte : valeur et unité, affichage, heure d'observation et de calcul, validité, source, algorithme,
qualité (`measured`, `derived`, `estimated`, `calibrating`, `incomplete`, `stale`, `missing`) et raison d'absence.

## 3. Contextes et transitions

Les événements observés passent avant l'horloge :

| Transition | Déclencheur | Repli |
|---|---|---|
| Nuit → Matin | réveil confirmé (Smart Wake), éveil soutenu ≥ 20 min (état du bracelet), ou nuit analysée | à partir de 04:00 sans aucune donnée, matin « en attente » jusqu'à 11:00 |
| Matin → Journée | 90 min après le réveil | 11:00 |
| Journée → Soir | 60 min avant l'heure de décrocher, au plus tard 20:00 | 20:00 |
| Soir → Nuit | endormissement soutenu ≥ 20 min (pas une sieste : bloc entre 09:00 et 19:00 de moins de 3 h) | 02:00 |

Hystérésis : une nuit sans nouvelle donnée (Bluetooth coupé, bracelet retiré) reste une nuit jusqu'à 11:00 ; un réveil
nocturne court (< 20 min) ne termine pas la nuit ; un soir commencé n'est pas annulé par un recalcul de quelques minutes.
Toutes les heures locales utilisent le fuseau courant (`utc_offset_seconds`), ce qui couvre les changements de fuseau.

La timeline précalculée contient l'entrée actuelle, chaque passage d'une valeur de « en direct » à « à HH:MM » puis
à périmée, l'instant où l'instantané devient trop ancien (6 h : « Données non actualisées depuis HH:MM » sur les cartes
Matin et Journée), puis les contextes suivants. Le matin et la journée du cycle suivant sont **en attente** : aucune
valeur de la veille n'est reprise. Après 24 h sans actualisation, le widget demande d'ouvrir l'app.

## 4. Planificateur du coucher

`coucher = réveil − (besoin de ce soir + délai d'endormissement habituel)`, avec le besoin et le délai de
`smart_wake.plan` (le besoin contient déjà une part plafonnée de la dette : pas de rattrapage complet en une nuit).
Régularité : le coucher n'avance pas de plus d'**1 h** par rapport au coucher habituel ; le manque est affiché
(« 1 h 15 de moins que le besoin ») au lieu d'un horaire irréaliste. Décrocher = coucher − 15 min, comme dans l'app.
Le réveil vient toujours de la configuration Smart Wake.

## 5. Tests (`cargo test --release --lib daily_context`)

| Scénario | Vérifié |
|---|---|
| Journée normale | matin (récupération, sommeil vs besoin, VFC, pouls), passage en journée à réveil + 90 min, soir (décrocher 22:30, coucher 22:45, fenêtre 06:30–07:00) ; source, algorithme, qualité sur chaque métrique |
| Nuit courte et dette élevée | message « Nuit courte : 2 h 30 de moins que ton besoin » ; dette 4 h 20 ; coucher avancé avec la part de dette |
| VFC absente | VFC et récupération « missing » avec leur raison, jamais de valeur |
| Bluetooth coupé | FC ancienne jamais « en direct » ; passage live → « à HH:MM » → périmée dans la timeline ; FC absente expliquée |
| Séance intense | « Course · 35 min », message « la FC élevée vient de l'effort, pas du stress » ; stress = repos immobile |
| FC élevée hors effort | stress 81, 45 min de stress élevé au repos |
| Horaires irréguliers | coucher limité à 1 h avant l'habitude, soir décalé en conséquence |
| Smart Wake modifié | réveil et coucher suivent le réglage, jamais modifié ; « réveil non programmé » si désarmé |
| Passage de nuit | nuit sur endormissement, réveil nocturne court et coupure Bluetooth sans effet, matin sur réveil confirmé, sieste ≠ nuit, matin suivant « en attente » |
| Instantané périmé | carte marquée « non actualisée » après 6 h, aucune valeur live ; nuit incomplète affichée en borne inférieure |
| Changement de fuseau | horloge locale du nouveau fuseau |

## 6. Limites

- **Signature** : l'App Group doit figurer dans les profils de provisionnement. Avec l'équipe personnelle gratuite,
  Xcode doit être connecté au compte Apple (Réglages › Comptes) pour les régénérer.
- Les formats écran verrouillé n'ont pas pu être vérifiés visuellement dans le simulateur.
- Le widget n'est à jour que si l'app a tourné récemment (synchronisation Bluetooth en arrière-plan) ; sinon il le dit.
- Le stress « récent » demande des périodes immobiles ; les jours analysés avant ce changement n'ont que la moyenne.

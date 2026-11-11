# Audit du comportement du bracelet WHOOP 5.0 (firmware 50.38.1)

**Source** : export iCloud complet des trames reçues (`whoop-lab/raw`), soit 173 988 trames du 9 novembre 21 h 05 au
11 novembre 11 h 35 (environ 38 h, 2 nuits), avec l'heure de réception sur l'iPhone. Tous les chiffres viennent de ces
données.

## 1. Ce que le bracelet produit seul, sans commande

| Flux | Rythme | Contenu | Conservation |
|---|---|---|---|
| **R18** (historique 124 o) | **1 enregistrement/s quand il est porté** | FC, R-R, gravité, accélération dynamique, pas, classe d'activité, état du bracelet, peau + 2 températures internes, SpO₂ ou code, indicateurs cardiaques, champs FC du firmware | en mémoire jusqu'au transfert |
| **R26** (PPG compact 88 o) | **rafales de 40 s toutes les ~19,4 min** (p10–p90 : 19,3–20,7), jour et nuit | 25 échantillons/s, 1 voie | historique |
| **SpO₂ à bord** (R18 octet 82) | **fenêtres de 30 s toutes les 20 min, à partir de l'endormissement détecté par le bracelet** | chaque seconde : SpO₂ 70–100 % ou code d'état | historique |
| **Événements** (paquet 48) | à l'événement ou périodiques | voir § 4 | **plus de 5 mois** (le plus ancien est du 29 mai 2026) |
| **Journal texte** (paquet 50) | lors des connexions, commandes et changements d'état | sous-systèmes BLE, capteurs, sommeil, port, batterie | transmis avec l'historique |

**Pas d'enregistrement hors du poignet.** Entre WRIST_OFF (10 h 20) et WRIST_ON (10 h 56) le 11 novembre, il n'y a
**aucun enregistrement R18** (trou de 36 min). Au total, il manque 79 min sur 38 h, toutes dans 16 trous.
`record_index` saute 1 554 numéros : ce sont les secondes non enregistrées, pas des trames perdues pendant le transfert.

### Calendrier de la SpO₂ à bord

| Nuit | Endormissement (bracelet) | Fenêtres | Première → dernière | Valides (≥ 10 s de %) | Médiane | Minimum |
|---|---|---|---|---|---|---|
| 9 → 10 | 01:16 | 24 | 01:36 → 09:16 | 8 | 99 % | 97 % |
| 10 → 11 | 23:42 | 31 | 00:02 → 10:02 | 14 | 94,5 % | 76 % |

- Les fenêtres tombent **20 min après l'endormissement, puis toutes les 20 min**. Elles ne sont pas calées sur l'heure.
- Codes d'état les plus fréquents : 2 (282 s), 160 (274), 32 (273), 128 (73), 16 (44), 3, 8, 168, 144, 24. Leur sens
  exact est inconnu : vraisemblablement « en cours », « signal insuffisant », « mouvement »…
- **La 2e nuit contient 7 fenêtres basses (76–93 %), alors qu'il n'y a aucun mouvement** (accélération max ≤ 0,02 g) et que
  le bracelet est en état « sommeil ». Voir § 5 : le mode optique est resté celui du jour cette nuit-là. **Ces valeurs ne
  doivent pas être interprétées médicalement sans contrôle à l'oxymètre.**

### Modes optiques (mot d'état R26 `status_raw`, aussi présent dans R18 et `STRAP_CONDITION_REPORT`)

| Période | Mode |
|---|---|
| 9 novembre, soir (éveil) | 2816 |
| nuit 9 → 10 | 2656 à l'endormissement (01 h), puis 2832 jusqu'au réveil |
| 10 novembre, matin | 2816 / 2896 / 2928 en quelques minutes |
| 10 novembre, journée → 11 novembre, matin, **nuit comprise** | **2896 en continu** (2464 une fois à 8 h) |

La 1re nuit, le firmware **change de configuration optique** à l'endormissement (le journal du firmware dit
`SENSORS: AFE configuration changed`). La 2e nuit, il ne l'a pas fait. La seule différence connue entre les deux nuits est
le réveil intelligent armé : FC en direct activée au coucher, alarmes programmées, IMU en direct à partir de 6 h 10.
**Hypothèse** : un flux temps réel actif empêche le passage en configuration nuit, ce qui dégraderait la SpO₂. À
vérifier par une nuit sans réveil intelligent armé.

## 2. Ce que le bracelet produit seulement sur commande

| Commande | Effet observé | Réponse |
|---|---|---|
| `TOGGLE_REALTIME_HR` (3) | paquet 40, **0,98 trame/s** | succès (25/25) |
| `TOGGLE_IMU_MODE` (106) | paquet 43 R21, **1 trame/s** (100 échantillons/axe) | succès |
| `ENABLE_OPTICAL_DATA` (107) + `TOGGLE_OPTICAL_MODE` (108) | paquet 43 R20, 0,95 trame/s | succès |
| `TOGGLE_PERSISTENT_R20` (153) | R20 dans l'historique (798 trames) | succès |
| `SEND_R10_R11_REALTIME` (63) | rien | **code 3 (refusé)** |
| `ENTER_HIGH_FREQ_SYNC` (96) | événement 97 ; aucune poussée observée | 0 à 15–60 s, 1 à ≥ 120 s |
| `SET_ALARM_TIME` (66) | `STRAP_DRIVEN_ALARM_SET`, puis `…_EXECUTED` + `HAPTICS_TERMINATED` à l'heure dite | le résultat 0 apparaît aussi pour des paliers de rampe qui ont bien vibré : le code n'est pas un simple succès/échec |
| `DISABLE_ALARM` (69) | alarmes annulées | succès |

## 3. Transmission (synchronisation de l'historique)

- **Protocole d'une synchro** (réponses comptées) :
  1. `GET_HELLO` (121), presque toujours avec le code 2 ;
  2. `GET_DATA_RANGE` (451 ; code 2 dans 94 % des cas, code 1 sinon) ;
  3. `SEND_HISTORICAL_DATA` (436), puis rafales de R18 / R26 ;
  4. `HISTORY_END` (métadonnée 2, 2 514) acquittée par `HISTORICAL_DATA_RESULT` (1 193) ;
  5. `HISTORY_START` (29) et `HISTORY_COMPLETE` (1) sont rares.
- **470 synchros en 38 h** : intervalle médian **5,9 min** (1 min dans la fenêtre du réveil intelligent). Une synchro dure
  **2,9 s** (max 106 s, pour la première, qui récupère 2 740 enregistrements). Débit médian **114 enregistrements/s**
  (max 213).
- **Retard mesure → iPhone** : médiane **2,7 min**, 95e centile 6,1 min, maximum 45 min (au retour du bracelet).
- **Doublons** : 2,7 % des R18 sont reçus deux fois (dédoublonnés en couche B).
- **Horloge** : l'heure de réception dépasse l'horodatage du bracelet de **2,4 à 3,0 s** en médiane (FC, IMU, R20 en
  direct). C'est la latence BLE + reconstitution, plus un éventuel décalage d'horloge.
- **FC en direct la 2e nuit** : le paquet 40 cesse d'apparaître dans l'export à **23 h 22**, alors que les synchros
  continuent normalement toute la nuit (environ 3 600 R18 reçus chaque heure). On ne sait pas si le flux s'est arrêté ou
  si l'app a cessé d'enregistrer ces trames. À instrumenter (compteur dans l'app) avant de conclure.

## 4. Événements (paquet 48, valeurs uniques)

| Événement | Nombre | Rythme | Contenu utile |
|---|---|---|---|
| `STRAP_CONDITION_REPORT` (29) | 540 | médiane 50 min (10 min récemment) | ‰ batterie candidat, mot de mode optique |
| `GENERIC_FIRMWARE_EVENT` (123) | 183 | ~1,6 min | 8 o, inconnu |
| `BATTERY_LEVEL` (3) | 101 | ~32 min | **% (÷ 10) et tension (mV)** |
| `STRAP_DRIVEN_ALARM_SET / EXECUTED`, `HAPTICS_TERMINATED` | 71 / 18 / 16 | pendant les vibrations | traçabilité de chaque palier |
| `BLE_CONNECTION_DOWN / UP` | 47 / 42 | à la reconnexion | qualité de liaison |
| `EVENT_120` | 46 | **6 h** (multiples de 2 h) | 32 o, contient 7 200 s : résumé périodique |
| `EVENT_110` | 29 | ~3,75 h | 8 o, inconnu |
| `DOUBLE_TAP` (14) | 6 | geste | confirmation de réveil |
| `WRIST_ON / OFF` (9 / 10) | 6 / 5 | geste | port (les trous R18 coïncident) |
| `HIGH_FREQ_SYNC_ENABLED / DISABLED`, `EVENT_115`, `EVENT_124`, `EXTENDED_BATTERY_INFORMATION`, `BATTERY_PACK_INFO` | rares | | |

## 5. Signaux calculés par le bracelet

| Signal | Observé |
|---|---|
| **État du bracelet** | éveil 38,1 %, immobile 10,2 %, sommeil 50,6 %, levé 1,1 % du temps. Épisodes médians : éveil 23 min, immobile 8,5 min, levé 35 min ; **2 blocs de sommeil** (01:16, 499 min ; 23:42, 628 min, réveil final vers 10 h 10) |
| **Classe d'activité** | immobile 97,25 %, marche 2,74 %, course 0,01 % |
| **R-R présents** | sommeil 77 %, immobile 66 %, éveil 42 %, levé 21 % des secondes |
| **SpO₂** | § 1 |
| **FC de repos du firmware** | octet haut de `rr_adjacent` : 0 en activité, ~48–53 au repos |
| **Indicateurs cardiaques** | bit 5 ≈ immobile / FC accrochée ; bits 0, 4, 6 ≈ mouvement |
| **Bits « poignet »** (octet 81, bits 0–1) | 1 dans 30 % de l'éveil, 40 % du « levé », 3 % du sommeil ; FC médiane 61 contre 52 bpm ; bascule 16 406 fois : **ce n'est pas une détection de port** (celle-ci passe par `WRIST_ON/OFF`), plutôt un indicateur de mouvement ou de contact instable |
| **Température de peau** | présente 100 % du temps porté ; médiane sommeil 33,0 °C, éveil 32,2 °C, levé 30,7 °C |
| **Température interne 2** | sommeil 31,5 °C, éveil 30,2 °C, levé 27,5 °C : suit l'environnement (couette, extérieur) |

## 6. Batterie

- Charge du 29 mai : 8,9 % → 97 % en environ 2 h 10. Le rythme est rapide jusqu'à environ 60 %, puis ralentit :
  21 à 96 %/h selon la phase.
- **Décharge récente** (charge complète le 10 novembre à 4 h 38) : 99,7 % → 85,9 % le 11 à 9 h 39, soit **≈ 0,47 %/h**
  (0,25 à 0,66 %/h selon la tranche), **≈ 8,9 jours par charge**. La tranche avec IMU en direct (6 h 11 → 9 h 39) est à
  0,57 %/h.
- Ancien historique (mai–juin) : 0,08 à 0,43 %/h, avec des usages et réglages inconnus.

## 7. Conséquences pour l'architecture

1. **SpO₂** : la planification est relative à l'endormissement du firmware. La 2e nuit est suspecte : il faut une nuit
   **sans** réveil intelligent armé pour tester l'hypothèse du mode optique, et sinon n'activer les flux temps réel que
   près de la fenêtre de réveil.
2. **Réveil intelligent** : ne pas supposer que la FC en direct reste continue toute la nuit. Le moteur fonctionne quand
   même avec R18 (synchro toutes les 1 à 6 min). Il faut ajouter un compteur des trames 40 reçues par l'app pour trancher.
3. **Trous hors du poignet** : utiliser `WRIST_OFF/ON` pour marquer les périodes non portées (au lieu de « données
   manquantes »), dans les durées portées et l'énergie.
4. **Batterie** : afficher % et autonomie estimée, et mesurer le coût de chaque mode (IMU, R20) par différence de pente.
5. **Événements anciens** : le journal du bracelet garde des mois d'événements. La 1re synchro en reçoit des centaines
   vieux de plusieurs mois : ne pas les mélanger au jour courant (filtrer par date).
6. **Retard de 2,7 min en médiane** : acceptable pour l'analyse de la journée. Pour le temps réel, seuls les flux sur
   commande conviennent (FC, IMU, R20 : environ 1 s).

## 8. Cause du mode optique « jour » la 2e nuit (analyse du 11 novembre)

Chronologie des commandes et des changements de mode (mot d'état R18 / R26, ne sont gardés que les modes tenus ≥ 60 s) :

| | Nuit 1 (9 → 10) | Nuit 2 (10 → 11) |
|---|---|---|
| `TOGGLE_REALTIME_HR` | par moments 21 h 04 – 22 h 13, puis plus rien | actif tout la nuit (réveil intelligent armé à 22 h 31) |
| Passage en mode nuit | 00 h 46 (2816 → 2656), 02 h 03 (→ 2832) | jamais pendant le flux |
| Après l'arrêt des flux (6 h 59) | — | rendormi à 7 h 02, mode nuit à **7 h 56** (2896 → 2464) |

Causes écartées :
- **2896** est un mode de jour ordinaire : il est apparu le 10 à 10 h 27 sans aucun flux, après le réveil.
- **Tests capteurs du 10** (IMU, optique, R20, synchro haute fréquence) : chaque activation a reçu une désactivation
  réussie ; aucun réglage n'est resté actif.
- **Synchronisations toutes les 6 min** : présentes les deux nuits.

**Cause retenue** : tant que le flux FC temps réel est actif, le firmware garde sa configuration optique de jour et ne passe
pas dans sa configuration de nuit, celle de la SpO₂. Il y passe environ 30 à 55 min après l'arrêt du flux.

**Correction** (`SmartWake.swift`) : la FC temps réel et l'IMU ne sont plus activées au coucher, mais **5 min avant
l'ouverture de la fenêtre de réveil**. Avant, le moteur travaille sur R18, synchronisé toutes les 5 min. La nuit 1 montre
que l'app reste active en arrière-plan sans flux (22 synchros par heure).

Le test optique de nuit (R20 en historique, 30 min) pourrait avoir le même effet : il est optionnel et reste désactivé par
défaut.

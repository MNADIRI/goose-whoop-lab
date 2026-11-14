# Réveil intelligent WHOOP 5.0

Code :
- moteur : `Rust/core/src/smart_wake.rs` (méthodes du pont `smart_wake.update`, `smart_wake.replay`, `smart_wake.feedback`) ;
- app : `GooseSwift/SmartWake.swift` (contrôleur et écran Réveil, au design Forme : heure et fenêtre, bouton Programmer /
  Désactiver, réglages « Heure fixe » / « Fenêtre flexible » et anticipation maximale, état réel de la nuit — alarme du
  bracelet, secours iPhone, suivi, décision —, plan du soir, détails techniques repliés).

Les outils de test des capteurs (rampe de vibration, calibration silencieuse, tests mouvement et optique, session capteurs
guidée, test optique de nuit) ont été **retirés de l'app le 14 novembre**. Leurs résultats ci-dessous restent comme
historique ; la calibration et le test de vibration effaçaient aussi l'alarme de l'heure limite (`DISABLE_ALARM` global).

## 1. Ce qui est réellement disponible en direct (vérifié sur ce bracelet, firmware 50.38.1)

| Signal | En direct ? | Latence | Statut |
|---|---|---|---|
| FC + R-R (paquet 40, `TOGGLE_REALTIME_HR`) | oui, 1 Hz | ~1 s | vérifié (2 223 trames reçues) |
| Gravité, état du bracelet, FC, R-R (R18) | via synchro de l'historique | ≤ 2 min dans la fenêtre (synchro toutes les 2 min), 10 min avant | vérifié |
| Double tap (événement 14) | oui (paquet 48) | ~1 s | nom vérifié, réception de nuit non testée |
| PPG (R26) | non : rafales de 40 s toutes les 20 min | — | inutilisable en direct |
| IMU brute (R21), mouvement en direct (R10/R11) | jamais observés | — | non utilisés |
| Vibration (`SET_ALARM_TIME` 66, `RUN_ALARM` 68, `DISABLE_ALARM` 69) | commandes présentes dans goose | — | vérifiées le 10 novembre (voir § 4) |

Le flux FC à 1 Hz sert aussi à garder l'app active en arrière-plan toute la nuit (mode `bluetooth-central`).

## 2. Choix du modèle

Modèles publiés étudiés, et pourquoi ils ne conviennent pas ici :
- **Walch 2019** (accéléromètre + FC + horloge circadienne) : il faut l'accéléromètre en direct, qu'on n'a pas.
- **SleepPPG-Net et modèles PPG** : il faut un PPG continu ; on n'a que des rafales.
- **LSTM / TCN sur la VFC** (Radha 2019, etc.) : les poids publiés correspondent à de l'ECG ou à d'autres capteurs, et rien ne
  permet de les réentraîner sans données de polysomnographie.
- **YASA** : il faut un EEG.

Choix retenu : un **modèle de Markov caché à 4 états** (éveil, léger, profond, paradoxal) avec un **filtre avant causal**.
- C'est un filtrage bayésien exact, qui ne regarde jamais le futur.
- Une mesure manquante est simplement neutre dans le calcul.
- Les probabilités sont directement exploitables, et le calcul prend 44 ms pour une nuit entière, 0,03 ms par décision.

Mesures utilisées, par tranche de 30 s :
- FC, en écart à la nuit en cours ;
- ln RMSSD sur les 5 dernières minutes, calculé sur des battements réellement successifs, avec le contrôle qualité `hrv_qc` ;
- variabilité de la FC, en échelle logarithmique ;
- mouvement (variation de gravité au-dessus de 10 mg) et état du bracelet, tous deux venant de R18.

Vraisemblances :
- inspirées de la littérature : en sommeil profond, FC plus basse et VFC plus haute ; en paradoxal, FC plus haute et plus variable ;
  à l'éveil, du mouvement et une FC qui monte ;
- robustes : z bornés à ±4, mélange avec une loi large ;
- tempérées à 0,6, pour compenser la corrélation entre mesures.

Transitions :
- elles dépendent du temps endormi : le profond domine en début de nuit, le paradoxal en fin de nuit (pression de sommeil) ;
- elles dépendent aussi de l'**heure de réveil habituelle** (médiane des nuits passées) : la probabilité de s'éveiller monte
  dans la demi-heure autour de cette heure.

## 3. Décision : la durée de sommeil d'abord

Politique `duration-first.v1` (`POLICY_VERSION` dans le moteur). Tous les seuils ci-dessous sont des **choix
heuristiques**, documentés et versionnés ; aucun n'est validé sur des utilisateurs Goose, et le classificateur de stades
n'a pas de validation par polysomnographie. Ils ne prouvent pas qu'un réveil anticipé apporte un bénéfice.

**Modes.** « Heure fixe » : réveil à l'heure choisie, sans anticipation (`max_advance_min = 0`). « Fenêtre flexible » :
anticipation au plus égale à l'**anticipation maximale choisie** (10, 20, 30, 45 ou 60 min, 30 par défaut). Il n'y a plus
d'ouverture obligatoire : la fenêtre ne s'ouvre jamais avant l'endormissement estimé + le sommeil recommandé ce soir, ni
avant l'heure limite − anticipation maximale. Si le besoin n'est pas couvert, elle ne s'ouvre pas du tout.

**Abstention (heure limite conservée)** dès qu'une de ces conditions est vraie (`hold_reasons`) :

| Code | Raison affichée | Condition |
|---|---|---|
| `fixed_time` | heure fixe | anticipation maximale = 0 |
| `onset_unknown` | endormissement non détecté | aucun endormissement observé (sur des époques mesurées) |
| `short_night` | nuit courte | sommeil mesuré depuis l'endormissement < sommeil recommandé ce soir |
| `sleep_debt` | dette de sommeil élevée | dette sur 7 nuits complètes ≥ 3 h (`DEBT_HOLD_H`) |
| `low_coverage` | données insuffisantes | couverture de la nuit < 90 %, ou analyse de la nuit en cours déjà marquée `INCOMPLETE_LOWER_BOUND` |
| `data_gap` | données insuffisantes | couverture < 80 % entre 30 min avant l'ouverture possible et maintenant (époques « U » près de la fenêtre) |

Une époque sans mesure (équivalent live du code « U ») n'est **jamais** comptée comme sommeil, ni comme éveil. Les
5 dernières minutes sont exclues du calcul de couverture parce que R18 n'arrive qu'à la synchronisation suivante.

**Arbitrage dans la fenêtre** (si aucune abstention) :
1. **Coût d'un réveil immédiat** : somme des probabilités d'état pondérées par un coût d'inertie supposé (profond 1,0,
   paradoxal 0,45, léger 0,15, éveil 0) + une pénalité α par minute de sommeil perdue avant l'heure limite, plus forte
   avec la dette et tant que le besoin n'est pas couvert. Ces poids ne sont pas une conversion validée.
2. **Valeur de l'attente** : coût attendu d'une règle réalisable (se réveiller au premier moment favorable détecté — 50 %
   par tranche en éveil/léger, 3 % de faux déclenchements en profond/paradoxal — sinon à l'heure limite), **calculé de façon
   déterministe** en propageant la masse de probabilité non encore « arrêtée » dans le modèle de transition. L'ancien
   tirage Monte-Carlo (256 trajectoires) faisait varier la décision d'un appel à l'autre sur les mêmes données.
3. **Marge minimale** : réveil seulement si coût immédiat + 0,003 (`MIN_BENEFIT`) ≤ valeur de l'attente.
4. **Persistance** : la décision de réveil et un état favorable (P(léger) + P(éveil) ≥ 0,6) doivent tenir sur les
   **2 dernières tranches** (1 min), toutes deux mesurées. Un seul epoch léger ne suffit pas.
5. **Données fraîches** obligatoires (FC de moins de 2 min), sauf à l'heure limite.

**Journal.** Chaque appel (toutes les 30 s) écrit dans `smart_wake_log` la décision complète, **attentes comprises** :
`policy.outcome` (`wait`, `fire`, `stimulating`, `probable_awake`), `policy.reason` (code + libellé), raisons
d'abstention, persistance, couverture. L'app journalise en plus chaque changement de raison.

**Probabilité exposée.** `p_reach_light_or_wake_10min` est une **probabilité d'atteinte** : la probabilité de passer au
moins une fois par le sommeil léger ou l'éveil dans les 10 min, sous le modèle de transition (premier passage). L'ancienne
sortie `p_light_or_wake_next_10min` prenait le maximum des probabilités marginales, ce qui n'est pas la probabilité d'au
moins une occasion favorable ; elle est supprimée. Cette valeur reste dans le journal et n'est plus affichée.

## 4. Vibration, progression et sécurité

La vibration a été vérifiée sur le bracelet le 10 novembre avec le motif WHOOP.

**Format du motif.** Il contient 8 effets, un mot de boucle par effet (u16), une boucle générale et une durée. Ce format
correspond au séquenceur d'une puce TI de type DRV2625 avec sa bibliothèque standard. Le motif WHOOP `[47, 152]` se lit
alors « vibration 100 % + pause 240 ms », c'est-à-dire des impulsions. Cette lecture est une hypothèse : le bracelet
répond `VALID_PATTERN`, `INVALID_EFFECT` ou `INVALID_LOOPS` à chaque motif, et la réponse était affichée par l'ancien écran de test.

**Calibration silencieuse du 10 novembre** (ancien bouton « Calibrer » : alarmes programmées dans 6 h puis annulées, aucune
vibration). Le bracelet accepte tous les effets testés (64, 118, 119–123, séquence montante, 47/152) et tous les mots de
boucle (0, 0x5555, 0xFFFF). **Seule contrainte trouvée** : avec la boucle générale à 7 (continue), la durée doit être d'au
moins 30 s ; 20, 15, 10 et 5 s sont refusées (`INVALID_DURATION`). Avec une boucle générale finie (0 à 3), 8 s est
acceptée. C'est pourquoi la première rampe, en boucle continue avec des paliers de 5 à 9 s, était refusée.

**Rampe continue** (modulation continue, sans impulsions ni pauses) :
- Chaque palier est un motif à niveau constant et sans trou : 8 fois le même effet, chacun joué 4 fois (mot de boucle
  `0xFFFF`), boucle générale 3, durée de 6 s (5 s plus 1 s de recouvrement).
- Niveaux : 10, 20, 30, 40, 50, 60, 80 puis 100 %, un palier toutes les 5 s, soit 0 → 100 % en 40 s. Les niveaux 70 % et
  90 % n'existent pas dans la bibliothèque d'effets.
- Le maintien à 100 % se fait ensuite par tranches de 5 s.
- Les paliers sont programmés sur l'horloge du bracelet, en alternant les alarmes 2 et 3.
- Si le bracelet refuse quand même un palier, l'app joue à la place la séquence montante validée d'un seul bloc.

| Niveau | Rampe | Maintien |
|---|---|---|
| 1 | 10 → 100 % en 40 s | 20 s |
| 2, si pas de réveil 120 s plus tard | 40 → 100 % en 25 s | 20 s |
| 3, encore 120 s plus tard | 60 → 100 % en 15 s | 40 s |
| Heure limite | alarme du bracelet (id 1) : séquence montante 10 → 100 % répétée pendant 60 s, sans téléphone | — |

Filet de sécurité supplémentaire : une notification iOS avec son à l'heure limite + 1 min.

**Réaction, éveil probable, arrêt confirmé.** Trois niveaux de preuve, jamais confondus :

| État | Indices | Effet |
|---|---|---|
| Réaction (`evidence.reaction`) | un seul indice : hausse de FC ≥ 12 bpm sur 60 s, **ou** P(éveil) ≥ 0,8 sur deux tranches mesurées | journalisée ; la progression continue, l'alarme de l'heure limite reste |
| Éveil probable (`probable_awake`) | état « levé » (UP), marche/mouvement soutenu en IMU direct (70 s après la vibration), ou FC **et** état d'éveil concordants ; levé ou marche avant l'alarme dans la fenêtre | rampe en pause ; **l'alarme du bracelet à l'heure limite reste programmée** ; rendormissement (P(éveil) < 0,3 sur 4 tranches mesurées, sans mouvement, après 5 min) → nouvelle stimulation |
| Arrêt confirmé (`awake`) | bouton « Je suis réveillé » ou double tap | tout s'arrête, y compris l'alarme de l'heure limite |

- L'arrêt explicite fonctionne sans capteur et sans attendre le moteur : l'app désarme tout de suite et informe le
  moteur ensuite.
- Sans confirmation 30 min après l'heure limite, la session se termine (`timeout`).
- **Un silence BLE n'est jamais un éveil** : une époque sans mesure ne compte pour aucun indice, et le test
  `silence_after_the_vibration_is_never_read_as_awake` le vérifie (la progression continue).

Pendant la phase de réveil, l'historique est synchronisé toutes les 45 s, entre deux rampes, pour voir passer l'état
« levé » rapidement.

**Fenêtre.** Tu choisis l'heure limite et le mode. En fenêtre flexible, elle s'ouvre à l'endormissement estimé + le
sommeil recommandé pour ce soir, jamais plus tôt que l'heure limite − anticipation maximale, et seulement si aucune
condition d'abstention n'est vraie. Pas d'ouverture forcée avant l'heure limite.

**Indépendance de l'heure limite.** L'alarme de l'heure limite ne dépend pas de l'app en arrière-plan :
- l'alarme du bracelet (id 1) est envoyée à l'armement et renvoyée jusqu'à la réponse `SUCCESS`, seulement tant que
  l'heure limite est à venir (une heure passée serait programmée pour le lendemain) ;
- si un `DISABLE_ALARM` part pendant une session (arrêt de la rampe), l'alarme limite est marquée à renvoyer ;
  les outils de test de vibration et de calibration ont été retirés de l'app ;
- un palier de rampe envoyé en retard est abandonné au lieu d'être reporté au lendemain ;
- la notification iOS de secours est programmée à l'armement (heure limite + 1 min) ; l'app affiche si elle est
  programmée ou non autorisée. Elle reste une notification ordinaire, non garantie en mode silencieux.

**Limites connues, non traitées ici** : confirmation de l'alarme limite déduite du dernier résumé de réponse
(`lastAlarmResponseSummary`) et non d'une réponse corrélée à la commande ; annulation uniquement globale
(`DISABLE_ALARM`) ; pas d'AlarmKit.

**Planificateur du coucher** (`smart_wake.plan`). Sommeil recommandé = somme de :
- la base personnelle : 75e centile des 14 dernières nuits, 8 h par défaut ;
- l'activité : charge des 2 derniers jours par rapport à ta norme, jusqu'à 45 min ;
- 25 % de la dette de sommeil des 7 dernières nuits, jusqu'à 60 min ;
- le rebond de sommeil profond : la moitié du déficit de la nuit dernière par rapport à ta médiane (60 min par défaut),
  jusqu'à 20 min.

Le total est borné entre 7 h et 10 h. On en déduit :
- l'heure de coucher : heure limite − sommeil recommandé − délai d'endormissement habituel ;
- le conseil de décrocher 15 min avant ;
- un avertissement si ce coucher tombe plus de 1 h 30 avant ton coucher habituel ;
- le réveil prévu si le plan est suivi, et le sommeil possible si tu te couches maintenant.

## 5. Personnalisation, protégée contre le surapprentissage

- **Repères** : FC et VFC de sommeil habituelles et heure de réveil habituelle, tirées des nuits passées.
- **Vraisemblances** : réajustées sur jusqu'à 10 nuits passées, avec un rétrécissement vers le modèle par défaut équivalent à
  600 tranches (environ 5 nuits d'un même état). Le modèle reste donc presque inchangé tant qu'il y a peu de nuits.
- **Coûts d'inertie** : réappris à partir des avis du matin (1 = groggy, 5 = frais), seulement après 8 matins notés, et
  rétrécis vers les valeurs de départ.

## 6. Évaluation reproductible

`smart_wake.replay` rejoue chaque nuit passée **comme en direct**, avec les données R18 servant de flux direct, une décision
toutes les 30 s et une fenêtre de 30 min avant le réveil réel. Il produit :
- la justesse, le score de Brier, la perte logarithmique, la courbe de fiabilité et la matrice de confusion, comparés à une
  référence ;
- le moment du réveil, l'état de référence à ce moment et l'état qu'aurait trouvé un réveil fixe à l'heure limite ;
- le temps de calcul ;
- la robustesse quand on retire 30 % ou 60 % des secondes.

La référence par défaut est l'hypnogramme calculé sur toute la nuit, qui est **lui-même une heuristique** et non une
polysomnographie. Le rejeu accepte aussi des étiquettes externes (`labels: [[ts, état]]`), ce qui permet de l'évaluer sur un
jeu de données avec polysomnographie (par exemple PhysioNet *sleep-accel*, en adaptant les signaux).

Résultat sur la nuit du 9 au 10 novembre (une seule nuit, personnalisation inactive) :

| Données retirées | Justesse vs référence | Brier | Perte log | Réveil choisi | État de référence à ce moment |
|---|---|---|---|---|---|
| 0 % | 0,43 | 0,87 | 1,80 | 7 min avant l'heure limite (léger estimé à 96 %) | éveil |
| 30 % | 0,37 | 0,97 | 1,99 | 6 min avant | éveil |
| 60 % | 0,36 | 0,98 | 2,01 | 6 min avant | éveil |

Ce qu'on peut en retenir :
- Le moteur fait surtout la confusion entre léger et profond. C'est la limite connue sans EEG, et la référence elle-même
  est incertaine.
- Il reste trop sûr de lui sur les probabilités hautes.
- **La qualité réelle du réveil ne se mesure qu'avec les avis du matin**, c'est l'indicateur final.

## 7. À vérifier sur le bracelet

1. La gestion de deux alarmes en parallèle.
2. La réception des double taps la nuit.
3. La tenue en arrière-plan toute la nuit : flux FC en direct et autonomie du bracelet avec le flux allumé.

## 8. Vérification (10 novembre)

**Bugs trouvés et corrigés** :
- **Le paquet 40 n'atteignait pas le moteur.** Le circuit d'analyse des trames ne tirait la FC que des paquets K10 et
  envoyait les trames 40 vers un traitement de fond. Le moteur ne recevait donc ni la FC ni les intervalles R-R en direct.
  Ils sont maintenant décodés dès l'arrivée de la trame (`forwardRealtimeHeartRate`), sans dépendre de ce filtrage.
- **Commandes bloquées pendant une synchronisation de l'historique.** La rampe pouvait rester silencieuse, l'alarme de
  l'heure limite et le flux en direct pouvaient ne pas être activés, et la désactivation pouvait échouer (l'alarme limite
  aurait alors sonné). Toutes ces commandes attendent désormais que le bracelet soit libre (`whenFreeForCommands`).
  L'alarme limite est renvoyée jusqu'à ce que le bracelet réponde `SUCCESS`, et le flux en direct est relancé s'il
  s'interrompt (après une reconnexion par exemple).
- **Pas d'horloge de secours.** Si le flux en direct ne démarrait pas, la boucle ne tournait plus du tout. Une horloge de
  15 s la relance.
- **Intervalles R-R comptés deux fois** si le profil FC standard et le paquet 40 sont actifs ensemble : une seule source
  est gardée.

**Tests de bout en bout** (`smart_wake::e2e`). Ils passent par le vrai point d'entrée `update` sur une base temporaire,
avec des appels cadencés comme dans l'app.

| Scénario | Résultat attendu et vérifié |
|---|---|
| Sommeil léger à partir de 6 h 35 (anticipation 30 min) | fenêtre = max(endormissement estimé + 8 h, limite − 30 min) ; réveil dans la fenêtre sur « sommeil léger persistant » ; « levé (UP) » 150 s après = éveil probable (alarme limite gardée) ; confirmation = arrêt |
| Même nuit, anticipation 0 (heure fixe) | aucun réveil avant l'heure limite ; chaque attente journalisée avec la raison `fixed_time` |
| Même nuit, analyse du soir `INCOMPLETE_LOWER_BOUND` | pas d'anticipation, raison « données insuffisantes » |
| 20 min sans données juste avant la fenêtre | pas d'anticipation (`data_gap`) jusqu'à l'heure limite |
| Couché à 3 h, limite 7 h | attente, raison « nuit courte » |
| Sommeil profond jusqu'au bout | attente jusqu'à l'heure limite, puis niveau 2 à +120 s, niveau 3 à +240 s, arrêt sur confirmation |
| Plus aucune donnée pendant la vibration | jamais « éveil probable » ni « réveillé » ; la progression continue |
| Levé à 3 h (hors fenêtre) | la session continue |
| Levé à 6 h 30 (dans la fenêtre) | éveil probable sans vibration, alarme limite conservée |
| Plus de données à partir de 6 h | aucun déclenchement avant l'heure limite, données marquées périmées |

Tests unitaires : probabilité d'atteinte ≥ maximum des marginales, décision déterministe, règles d'abstention, persistance
(un seul epoch léger ne déclenche pas), `U` jamais REM, nuit incomplète sans dette.
| Planificateur (7 nuits simulées) | base 420, profond +20, dette 0, activité > 0 ; somme = recommandation ; coucher = limite − (sommeil + délai d'endormissement) |

**Sur la vraie nuit du 9 au 10 novembre** :
- Un appel complet d'`update` prend 40 à 70 ms sur le Mac (ingestion, nuit entière, décision), toutes les 30 s.
- À l'heure limite, l'action est `stimulate`. L'endormissement est estimé à 22 h 37.

**Reste à vérifier en conditions réelles** (une nuit avec l'alarme activée) :
- que les paquets 40 arrivent bien en arrière-plan toute la nuit ;
- que l'alarme limite et les alarmes de la rampe (id 1 à 3) peuvent coexister ;
- le délai entre le lever et l'arrêt, qui dépend de la synchronisation toutes les 45 s.

## 9. Mouvement en direct (tests du 10 novembre)

Test silencieux « Mouvement en direct » (ancien outil de l'écran Alarme), réponses du bracelet :

| Méthode | Réponse | Données reçues |
|---|---|---|
| Synchronisation haute fréquence (commande 96) à 15, 30 et 60 s | refusée | — |
| Synchronisation haute fréquence à 120 et 180 s | acceptée (événements 97 / 98) | un envoi au mieux toutes les 2 min, pas mieux que la synchronisation actuelle : non utilisée |
| Flux mouvement temps réel R10/R11 (commande 63) | refusé (code 3) | — |
| **Mode IMU temps réel (commande 106)** | **acceptée** | **paquet 43, même format que R21 (1 244 o), 1 trame par seconde, 100 échantillons par axe** |

**Utilisation pour le réveil.** L'app active le mode IMU **5 min avant l'ouverture de la fenêtre**, pour épargner la
batterie, puis le relance s'il s'interrompt et le coupe à la fin. Les trames brutes sont gardées telles quelles
(`smart_wake_imu`) et décodées par le décodeur R21. Elles fournissent, avec environ 1 s de retard :
- l'orientation du poignet à chaque seconde, qui remplace la gravité R18 arrivant 2 min plus tard ;
- l'intensité du mouvement : écart-type de |a| sur la seconde ;
- les pas, via `imu::features`.

**Nouvelles règles :**
- **Marche dans la fenêtre** (au moins 20 pas en 30 s) : réveil acquis, sans vibration.
- **Après la vibration** : au moins 10 pas, ou au moins 15 s de mouvement (écart-type > 0,06 g) dans les 30 dernières
  secondes, suffisent à arrêter l'alarme. Les 70 s qui suivent le déclenchement sont ignorées, car le moteur haptique
  secoue lui-même l'accéléromètre.

Ces règles sont testées dans `smart_wake::e2e` avec de vraies trames 43 synthétiques. Il reste à vérifier sur des données
réelles l'échelle de l'accéléromètre (1/4096 g supposée, contrôlable avec `imu::accel_scale_check`) et l'autonomie du
bracelet avec le mode IMU allumé environ 50 min.

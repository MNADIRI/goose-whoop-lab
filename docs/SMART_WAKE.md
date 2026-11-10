# Réveil intelligent WHOOP 5.0

Code :
- moteur : `Rust/core/src/smart_wake.rs` (méthodes du pont `smart_wake.update`, `smart_wake.replay`, `smart_wake.feedback`) ;
- app : `GooseSwift/SmartWake.swift` (contrôleur et carte « Réveil intelligent » de l'écran Aujourd'hui).

## 1. Ce qui est réellement disponible en direct (vérifié sur ce bracelet, firmware 50.38.1)

| Signal | En direct ? | Latence | Statut |
|---|---|---|---|
| FC + R-R (paquet 40, `TOGGLE_REALTIME_HR`) | oui, 1 Hz | ~1 s | vérifié (2 223 trames reçues) |
| Gravité, état du bracelet, FC, R-R (R18) | via synchro de l'historique | ≤ 2 min dans la fenêtre (synchro toutes les 2 min), 10 min avant | vérifié |
| Double tap (événement 14) | oui (paquet 48) | ~1 s | nom vérifié, réception de nuit non testée |
| PPG (R26) | non : rafales de 40 s toutes les 20 min | — | inutilisable en direct |
| IMU brute (R21), mouvement en direct (R10/R11) | jamais observés | — | non utilisés |
| Vibration (`SET_ALARM_TIME` 66, `RUN_ALARM` 68, `DISABLE_ALARM` 69) | commandes présentes dans goose | — | **jamais envoyées à ce bracelet, donc à vérifier** (bouton « Tester la vibration ») |

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

## 3. Décision : arrêt optimal prédictif

1. **Coût d'un réveil immédiat** : la somme des probabilités d'état pondérées par l'inertie du réveil (profond 1,0,
   paradoxal 0,45, léger 0,15, éveil 0), d'après Tassi & Muzet 2000 et Trotti 2017. On y ajoute une pénalité α par minute de
   sommeil perdue avant l'heure limite. α augmente avec la dette de sommeil des 7 dernières nuits et si la nuit est encore
   loin du besoin habituel.
2. **Valeur de l'attente** : 256 trajectoires simulées avec le modèle de transition jusqu'à l'heure limite. Chaque trajectoire
   suit une règle réalisable : se réveiller au premier moment favorable détecté (détection à 50 % par tranche, 3 % de
   déclenchements à tort), sinon à l'heure limite.
3. **Réveil** si le coût immédiat est au plus égal à la valeur de l'attente. Il y a aussi réveil si la personne est déjà
   éveillée (P(éveil) ≥ 0,75 sur deux tranches) ou si l'heure limite est atteinte.
4. **Aucun réveil sur des données périmées** (FC de plus de 2 min), sauf à l'heure limite.

L'app affiche aussi la probabilité d'un moment favorable dans les 10 prochaines minutes, marquée comme **prédite**. Les
probabilités d'état sont marquées **estimées**, et les mesures **mesurées**.

## 4. Vibration, progression et sécurité

La vibration a été vérifiée sur le bracelet le 10 novembre avec le motif WHOOP.

**Format du motif.** Il contient 8 effets, un mot de boucle par effet (u16), une boucle générale et une durée. Ce format
correspond au séquenceur d'une puce TI de type DRV2625 avec sa bibliothèque standard. Le motif WHOOP `[47, 152]` se lit
alors « vibration 100 % + pause 240 ms », c'est-à-dire des impulsions. Cette lecture est une hypothèse : le bracelet
répond `VALID_PATTERN`, `INVALID_EFFECT` ou `INVALID_LOOPS` à chaque motif, et la réponse s'affiche dans l'onglet Alarme.

**Calibration silencieuse du 10 novembre** (bouton « Calibrer » : alarmes programmées dans 6 h puis annulées, aucune
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

**Arrêt.** Le réveil est considéré comme acquis, et la vibration s'arrête sans relance, dans ces cas :
- le bracelet passe à l'état « levé » (UP, réveil soutenu, levé orthostatique) dans la fenêtre ; s'il était déjà levé avant
  l'alarme, aucune vibration n'est envoyée ;
- un double tap ;
- le bouton « Je suis réveillé » ;
- une hausse de FC soutenue d'au moins 12 bpm sur 60 s (au moins 40 échantillons) par rapport aux 5 min avant la vibration ;
- P(éveil) ≥ 0,8 sur deux tranches.

Pendant la phase de réveil, l'historique est synchronisé toutes les 45 s, entre deux rampes, pour voir passer l'état
« levé » rapidement.

**Fenêtre automatique.** Tu ne règles que l'heure limite. La fenêtre s'ouvre à l'endormissement estimé + le sommeil
recommandé pour ce soir, en restant entre 45 et 10 min avant l'heure limite (30 min tant que l'endormissement n'est pas
détecté).

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

1. La vibration : bouton « Tester la vibration ».
2. La gestion de deux alarmes en parallèle.
3. La réception des double taps la nuit.
4. La tenue en arrière-plan toute la nuit : flux FC en direct et autonomie du bracelet avec le flux allumé.

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
| Sommeil léger à partir de 6 h 35 | fenêtre = endormissement estimé (22 h 20 ± 2 min) + 8 h ; réveil dans la fenêtre, pas à l'heure limite ; arrêt « levé (UP) détecté » 150 s après |
| Sommeil profond jusqu'au bout | attente jusqu'à l'heure limite, puis niveau 2 à +120 s, niveau 3 à +240 s, arrêt sur confirmation |
| Levé à 3 h (hors fenêtre) | la session continue |
| Levé à 6 h 30 (dans la fenêtre) | « réveillé » sans aucune vibration |
| Plus de données à partir de 6 h | aucun déclenchement avant l'heure limite, données marquées périmées |
| Planificateur (7 nuits simulées) | base 420, profond +20, dette 0, activité > 0 ; somme = recommandation ; coucher = limite − (sommeil + délai d'endormissement) |

**Sur la vraie nuit du 9 au 10 novembre** :
- Un appel complet d'`update` prend 40 à 70 ms sur le Mac (ingestion, nuit entière, décision), toutes les 30 s.
- À l'heure limite, l'action est `stimulate`. L'endormissement est estimé à 22 h 37.

**Reste à vérifier en conditions réelles** (une nuit avec l'alarme activée) :
- que les paquets 40 arrivent bien en arrière-plan toute la nuit ;
- que l'alarme limite et les alarmes de la rampe (id 1 à 3) peuvent coexister ;
- le délai entre le lever et l'arrêt, qui dépend de la synchronisation toutes les 45 s.

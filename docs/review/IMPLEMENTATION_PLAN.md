# Plan d'implémentation complet proposé à Opus

Ce document est une spécification de travaux restants, pas une liste de fonctionnalités livrées.
Les corrections déjà codées sont C01–C16 dans `OPUS_REVIEW.md`. Aucun résultat clinique n'est
revendiqué. Les chiffres de couverture, de fenêtre ou de budget ci-dessous sont des choix
techniques initiaux à tester, sauf lorsqu'ils décrivent un format observé du dépôt.

## A — Temps, décodage et intégrité (prérequis de tous les autres lots)

### A1. Un décodeur canonique par version firmware

Fichiers : `whoop5_records.rs`, `protocol.rs`, `whoop_lab_analysis.rs`, `whoop5_store.rs`.

Remplacer le décodage parallèle `parse_r18_history_fields`/offsets locaux par une projection de
`whoop5_r18`. Conserver payload brut et `decoder_version`. Si le décodeur change : créer une
nouvelle projection, vérifier nombre de lignes/champs/CRC, puis changer atomiquement la version
active. Ne pas effacer la preuve brute. Capturer des fixtures anonymisées de plusieurs firmwares.

Réception : les mêmes octets décodés en Python/Rust donnent les mêmes valeurs/unités ; les champs
non établis restent bruts ; la reconstruction reproduit les résultats à version constante.
Une ancienne ligne invalide ne survit pas à une migration déclarée complète.

### A2. Horloges et véritables segments continus

Introduire `device_tick`, `device_epoch`, `host_monotonic_received`, `host_utc_received`,
`boot_session_id`, `clock_model_id`, `clock_uncertainty_ms`, `record_index`, `sample_index`.
Estimer `t_reference = a * t_device + b` sur marqueurs de synchronisation ; ne pas confondre
latence de transfert et dérive. Les tableaux d'échantillons portent leur temps de début, fréquence
observée, nombre effectif et incertitude. Conserver les temps originaux après correction.

Réception : reboot, wrap d'index, paquets retardés, réordonnés, doublons et changement d'heure
ne fusionnent pas deux segments. Une coupure de cinq minutes ne devient jamais une fenêtre de
cinq minutes parce que 300 lignes subsistent.

### A3. Fuseau IANA et épisodes de sommeil

Faire évoluer `AnalyzeOptions` avec `timezone_id`, persister le fuseau au moment de la capture et
le contexte de voyage. Employer une bibliothèque de fuseaux versionnée ; journées civiles de
23/24/25 heures. Définir explicitement si les tendances sont en temps local historique ou actuel.
Ajouter `sleep_episodes` : sommeil principal, sieste, sommeil diurne et période inconnue.
Les fenêtres se définissent par épisodes, pas seulement 18 h→14 h et « plus long bloc ».

Réception : Bruxelles aux deux transitions DST, voyage avec changement de fuseau, sieste de
90 minutes, sommeil de jour après garde, interruption BLE nocturne. Ne pas imposer N3 dans
la première moitié ni REM après une heure comme preuve de stade.

### A4. Déduplication et conflits

Clé de capture : appareil pseudonymisé + session de démarrage + type + index + hash payload.
Même clé logique/même contenu : doublon. Même clé/contenu différent : conflit conservé, résolution
versionnée. Ne jamais supprimer une observation contradictoire par simple INSERT OR IGNORE.
Distinguer durée non portée, non acquise, perdue en transfert, rejetée CRC et rejetée par SQI.

Réception : reprise de sync idempotente, aucun trou masqué, provenance remontable jusqu'aux octets.

### A5. Cache incrémental optimisé

La branche utilise une invalidation globale conservatrice. Remplacer après profilage par un DAG :
partition signal→fenêtres→nuit/jour→baseline suivante→scores. Empreinte incluant données, règles,
profil, fuseau, décodeur, version d'algorithme et dépendances. Corrections rétroactives propagées.
Réception : arrivée tardive R26, correction température à timestamp identique, nouvelle annotation
et changement du poids utilisateur réactualisent exactement les sorties dépendantes.

## B — Orchestration des capteurs

### B1. Un arbitre partagé Swift/Rust

Remplacer les commandes indépendantes sport/sommeil/réveil/test par des demandes persistées :
`lease_id`, `owner`, `stream`, `mode`, `priority`, `expires_at`, `budget`, `reason`, `state`.
Distinguer `requested`, `command_sent`, `acknowledged`, `observed`, `stopping`, `stopped`, `unknown`.
Une requête expire même après redémarrage ; des demandes compatibles se partagent le flux.
Une révocation sport ne coupe pas une capture encore demandée par le protocole sommeil.

API proposée : `acquisition.request`, `acquisition.release`, `acquisition.reconcile`,
`acquisition.status`, `acquisition.cost_report`. Journal append-only des décisions et commandes,
avec séquence, réponse firmware, nombre de tentatives et preuve observationnelle.

### B2. Arrêt vérifié et reprise

Réutiliser la séquence observée dans `docs/SENSORS.md` :
153 off →108 off →107 off →154 off →106 off →63 off →3 off,
mais uniquement après arbitrage des autres propriétaires des flux concernés.
Un ACK n'est pas une preuve de fin. Vérifier les derniers timestamps de capture ; une trame
historique reçue tardivement n'est pas une preuve que le capteur tourne encore.

Enregistrer les intentions sur disque avant émission. À la reconnexion, comparer modes voulus,
logs AFE, échantillons observés et historique. Timeout + retry borné + état inconnu visible.
Ne pas supposer qu'iOS exécutera un timer nocturne lorsque l'app est suspendue. Chercher une
expiration embarquée ; si absente, documenter le risque et limiter les protocoles concernés.

Réception matériel : arrêt après crash, interruption Bluetooth, deux propriétaires simultanés,
ACK sans arrêt, batterie faible, historique en retard. Le flux R18 normal doit rester intact.

### B3. Budgets et adaptation

Mesurer batterie/heure, volume/heure, temps de sync et perte par configuration. Comparer R18 seul,
R21, R20 et R20+R21. Les tailles observées (R20 2140 B/s, R21 1244 B/s) donnent un ordre de grandeur,
pas une mesure énergétique. Pas de proposition d'augmentation ODR sans commande réellement prouvée.

Politique : quotidien frugal ; mesure guidée au repos ; activation IMU sur suspicion de sport avec
hystérésis ; enregistrement prolongé seulement quand utile ; nuit de recherche explicitement distincte.
Conserver des négatifs de pré/post-détection pour entraîner les futures têtes HAR.

### B4. Calibration et non-port

Conserver biais gyro, échelle accéléro, matrice d'orientation, température, saturation et sentinelles.
Une substitution de sentinel ne rend pas l'échantillon « observé ». Croiser indices de contact,
amplitude, DC, température et mouvement pour le non-port, sans assimiler immobilité à non-port.
Réception : bracelet sur table, lâche, serré, autre poignet, sous vêtement, changement thermique.

## C — Stockage, sorties, annotations et performance

### C1. Quatre couches conservées

1. Brut immuable, CRC et ordre de réception.
2. Décodé avec unités, configurations et masques.
3. Caractéristiques/époques avec fenêtres exactes et versions.
4. Résultats avec provenance, couverture, qualité et incertitude.

Enveloppe métrique : `id`, `value|null`, `unit`, `start`, `end`, `source`, `quality_status`,
`validation_status`, `coverage`, `exclusion_reasons`, `algorithm_version`, `decoder_version`,
`reference_validation_id|null`. Ne pas inventer un intervalle de confiance non calibré.
La compatibilité HealthKit conserve la distinction RMSSD/SDNN ; pas d'export d'un score en constante.

### C2. Signaux haute fréquence

SQLite pour index/événements/métadonnées ; blobs segmentés compressés ou fichiers chunkés pour
R20/R21. Exports Parquet pour séries/caractéristiques, manifest JSON et hash SHA-256 des fichiers.
Garder la composante DC, les canaux séparés et les changements AFE : ni normalisation destructive,
ni simple conservation d'embeddings, qui empêcherait un futur réentraînement.

Adapter `sport.export_dataset` et les exports nocturnes au sidecar de `capture_manifest.py` :
heure appareil/référence/réception, canaux, fs et origine de fs, configuration, bornes et masques.
Les labels contiennent source, annotateur pseudonyme, temps, incertitude et version de correction.

### C3. Backpressure et calcul

Persister la preuve avant les chemins UI susceptibles de drop. File acquisition séparée des
rafraîchissements visuels ; écritures batch WAL ; budget mémoire ; métriques de latence et dropped.
Calcul par fenêtres réutilisables, pas recalcul de toute l'histoire à chaque notification BLE.
Passer de DFT O(N²) à FFT O(N log N), comparer bin par bin sur signaux connus et énergie intégrée,
vérifier DC/Nyquist et PSD en unités physiques avant substitution.

### C4. Export et confidentialité

Exports automatiques explicites, configuration visible, destinations distinctes. Retirer numéro de
série et identifiants des captures partagées ; conserver mapping local. Chiffrement au repos/export
selon la destination, rotation des clés, sauvegarde et restauration testées. Pas de nouveau cloud
ou envoi d'information de santé par défaut. Le dépôt public contient du synthétique uniquement.

## D — Traitement PPG, PRV et respiration

### D1. Reconstruction PPG et SQI

Étendre R26 à R20 multi-voies. Séparer signal brut/DC/AC, appliquer offsets et ambiante seulement
après correspondance des voies démontrée. Segments séparés aux changements d'AFE, gain, LED,
photodiode, gap ou saturation. Ne pas attribuer vert/rouge/IR sur la seule amplitude.

SQI par fenêtre et par usage : `usable_for_hr`, `usable_for_prv`, `usable_for_morphology`.
Caractéristiques : saturation, flatline, sauts, périodicité, morphologie, accord des voies,
accord de détecteurs, mouvement, continuité. Calibration des décisions contre une référence.
Un champ firmware d'acceptation reste une observation avec degré d'interprétation, pas une vérité.

### D2. Pouls et artefacts de mouvement

Banc comparatif MSPTD/qppg/détecteur actuel ; séparation estimation FC et précision des battements.
Portage seulement avec tests de parité contre le code original et corpus PPG-DaLiA + WHOOP/ECG.
À 25 Hz, espacement 40 ms ; interpolation sous-échantillon ≠ récupération de bande passante.
Ne pas annoncer une précision milliseconde sans mesure d'erreur externe.

Mouvement : spectres PPG/accéléro, candidats multiples, harmoniques de cadence, suivi temporel,
fusion des voies, comparaison à NLMS/RLS et approche inspirée TROIKA. Un notch aveugle peut
supprimer le pouls lorsque FC et cadence se recouvrent. Abstention explicite si ambiguïté.
Filtres causaux en live ; filtfilt et contexte futur réservés au recalcul différé.

### D3. PRV et QC des intervalles

Formaliser la différence intervalle firmware / pulsation / R-R ECG. Prouver ordre et couverture des
battements dans R18/paquet40. Stocker temps du battement, source, confiance temporelle et corrections.
Ne pas concaténer des intervalles à travers un trou, même si les paquets semblent voisins.

Comparer le QC actuel à Lipponen–Tarvainen complet ; distinguer extra/missed/misaligned et
irrégularité inexpliquée. Ne pas « corriger » automatiquement une arythmie réelle en rythme normal.
Afficher taux de correction, taux de rejet et segments utilisables. RMSSD/lnRMSSD par 5 min valides,
médiane et dispersion nocturnes ; SDNN avec durée explicite. La mesure matinale exige un protocole
matinal réel ; les dix dernières minutes de sommeil ne sont pas une mesure au réveil.

Tachogramme sur temps des battements ; interpolation limitée ou analyse irrégulière ; fs cible
et anti-repliement explicites. LF/HF descriptif uniquement, VLF court terme non interprété cliniquement.

### D4. Respiration

Trois branches : modulation des intervalles, de l'amplitude PPG et de la ligne de base.
Choix de bandes en fonction du protocole, y compris respiration lente ; absence de pic fiable→null.
Fusion pondérée par SQI, cohérence entre sources et contexte mouvement. Vérifier avec ceinture
respiratoire/capnographie. R26 produit des observations ponctuelles, R20 continu une série dense.
Pas d'ODI/AHI ou apnée à partir d'une fréquence respiratoire moyenne et de spot SpO2.

## E — Activité et sommeil : capture d'abord

Les adaptations de capture et la tête future de chaque modèle sont dans `MODELS_AND_CAPTURE.md`.
Aucun nouveau déploiement HARNet/SleepPPG/PaPaGei n'est inclus sans satisfaire la règle d'exploitabilité.

### E1. HAR

Distinguer détection début/fin, classe du sport et structure interne. Prébuffer/postbuffer avec
négatifs, annotation des transitions et classe inconnu. Collecter le gyroscope en plus de l'accéléro.
Pour les poids HARNet, préserver axes/unités g, format 3×300 à 30 Hz sur 10 s pour le banc existant.
Évaluer aliasing, orientation et port ; ne pas imposer invariance à une rotation qui efface un geste.

Pour séries/répétitions : exemples humains par exercice et séance, repos, asymétrie et exercices
peu observables. Séparer comptage et reconnaissance. Ne pas appeler force/puissance de barre une
vitesse obtenue par intégration du poignet sans modèle mécanique et référence.

### E2. Sommeil

D'abord valider sommeil/éveil ; cibles 4 classes seulement avec référence adaptée.
Capture R18 + R20 continu si possible + R21 + logs AFE ; PSG/EEG alignés à 30 s ; qualité et gaps.
Former éventuellement une tête temporelle sur features R18 et une tête adaptée PPG, comparer
sur mêmes participants tenus à l'écart. N1/N2 fusionnés si qualité insuffisante pour cinq classes.

Réveil intelligent : modèle causal, deadline ferme, jamais de contexte futur utilisé en live.
La détection d'un stade « léger » n'est pas à elle seule une preuve d'amélioration de l'inertie
du sommeil ; protocole séparé avec auto-évaluation/temps de réaction si cette promesse est poursuivie.

## F — Métriques utiles et limites

| Mesure | Priorité / modification | Validation / limite |
|---|---|---|
| Couverture et temps porté | P0 par flux et par métrique | Dénominateurs explicites ; absence ≠ zéro |
| FC de repos | Standardiser condition et durée, conserver médiane nocturne | ECG ; minimum glissant identifié comme tel |
| PRV RMSSD/lnRMSSD | Fenêtres valides comparables, référence individuelle | Accord ECG, couverture et correction |
| Respiration nocturne | Fusion et écart personnel | Référence respiratoire |
| Température | Delta personnel + contact/ambiance/configuration | Peau, pas température centrale |
| Sommeil | TST, WASO, horaires, efficacité, siestes | PSG, gestion périodes inconnues |
| Régularité | SRI et dispersion circulaire des horaires | Comparaisons à 24 h avec jours valides, DST et siestes explicites |
| Activité | Pas, cadence, bouts actifs, immobilité | Vidéo ; immobilité poignet ≠ posture assise certaine |
| Charge cardio | Zones personnelles, TRIMP et session-RPE | Documenter FCmax réelle/estimée ; musculation ≠ charge FC seule |
| Récupération FC | 60/120 s avec protocole actif/passif | Mouvement, posture et instant d'arrêt |
| Dérive cardio | À allure/puissance externe comparable | GPS/power nécessaires ; FC seule insuffisante |
| Stress | Renommer activation physiologique au repos | Cause psychologique non identifiable par FC seule |
| Calories | Estimation, incertitude et type d'activité | Calorimétrie pour validation, pas exactitude affichée artificielle |
| VO2 ratio FC | Exploratoire, pas mesure de VO2 | Test d'effort/échanges gazeux pour cible réelle |
| ACWR | Descriptif recherche, aucune prescription anti-blessure | Pas de seuil « zone sûre » |
| Recovery/fatigue | Composantes exposées ; score défini par une cible | Labels prospectifs fatigue/performance ; pas vérité biologique universelle |
| Changement longitudinal | Baseline robuste + qualité + contexte | Faux positifs/semaine et valeur prédictive prospective |
| SpO2 | Champ firmware candidat et statistiques ponctuelles | Valider décodage et accord ; pas nadir nocturne/ODI |
| PA | Aucune valeur proposée | Quelques mesures au brassard ne valident pas une tendance personnelle |
| Arythmie | Préserver les irrégularités pour recherche | ECG indispensable avant toute revendication diagnostique |

Baselines : fenêtre/horizon déclarés, EWMA robuste, nombre de nuits valides, état provisoire/périmé,
retour après interruption, gestion changement de matériel et exclusion de contamination par la
fenêtre testée. La renormalisation d'un score lorsqu'une composante manque doit être visible,
car elle change la signification du nombre. Aucun score d'infection/surentraînement automatique
avant protocole prospectif défini.

## G — Validation et réception

- Signal : ECG, respiration, oxymétrie ; erreur, biais, limites d'accord, couverture et abstention.
- Sommeil : PSG, macro-F1/kappa et rappel éveil, erreurs TST/WASO/latence ; bootstrap par sujet.
- HAR : macro-F1/classes, fausses séances/jour, erreur début/fin, reps/série ; splits par groupes.
- Acquisition : crash/reconnect, batterie, pertes, latence, saturation stockage et reprise.
- Stratification : phototype/pigmentation mesurée lorsque disponible, position, serrage, mouvement,
  âge et contexte clinique, sans prétendre valider des sous-groupes trop petits.
- Test final gelé avant réglages ; n de sujets/nuits déterminé par précision cible de l'intervalle
  de confiance, pas par un nombre arbitraire suffisant pour « valider ».
- CI signal synthétique + fixtures dé-identifiées ; capture privée jamais commitée.

Ordre proposé : A→B/C→D→E→F, avec G dès le premier lot. Une démonstration d'interface, un test
unitaire ou un accord avec le firmware ne remplace aucune référence physiologique indépendante.

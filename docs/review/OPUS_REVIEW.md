# Revue physiologique Goose — point d'entrée pour Opus

Date : 2026-11-13. Base auditée : `4a4cc28bd8e54fe3950ce25fdb3a894f500093b9`.
Rebasée sur `b5ac3b4` : nouvelle interface Forme et score composite amont conservés, garde de couverture appliquée.
Branche proposée : `codex/physiology-review`. **Ne pas fusionner sans revue.**

## Mandat

Préparer un wearable de santé à partir des signaux réellement accessibles du WHOOP 5.0.
L'utilisateur demande des corrections et des propositions retrouvables dans le dépôt, à réviser
par Opus. Aucun modèle appris n'est proposé comme directement déployable sans preuve que son
checkpoint, sa tête, son prétraitement et ses sorties répondent à la tâche cible.
Pour les autres modèles, le périmètre est la préparation des captures, du stockage et des labels,
avec identification de la tête à entraîner. Aucun poids n'est téléchargé ni nouvelle inférence
neuralement produite par cette branche.

## Où commencer

1. Lire ce fichier et `VALIDATION.md` pour distinguer code, tests et hypothèses.
2. Examiner `git diff b5ac3b4...HEAD` et exécuter les commandes de validation ci-dessous.
3. Examiner `IMPLEMENTATION_PLAN.md` : chaque chantier non livré contient les interfaces et critères d'acceptation.
4. Examiner `MODELS_AND_CAPTURE.md` : statut d'exploitabilité et données pour chaque future tête.
5. Lire la section « Réveil intelligent — cahier des charges technique et UI/UX » ci-dessous ; SW01 est bloquant avant fusion.
6. Valider sur une **copie** de base réelle et sur iPhone ; les tests synthétiques ne valident pas la physiologie.

## Changements implémentés

| ID | Fichiers | Modification | Limite / point de revue |
|---|---|---|---|
| C01 | `whoop_lab_analysis.rs`, vues Swift | Époque <80 % de couverture R18 ou gravité → `unknown`, encodée U ; pas de lissage à travers U | 80 % est une politique initiale à valider, pas un seuil scientifique universel |
| C02 | `whoop_lab_analysis.rs` | RMSSD local des stades réutilise les segments et QC communs | Continuité paquet ≠ continuité battement prouvée ; à valider sur ECG |
| C03 | `whoop_lab_analysis.rs` | Retrait des réétiquetages N3/REM par le faux CPC | Les règles restantes demeurent expérimentales |
| C04 | `health_indices.rs`, Swift | `cpc` public = null ; caractéristiques déplacées vers `rr_spectral_pattern` ; retrait de « sommeil stable » | Fonctions internes cpc conservées pour compatibilité de code, sans revendication CPC |
| C05 | `health_indices.rs`, Swift | Aucun score respiratoire 0–100 ni ODI à partir de SpO₂ ponctuelle | Statistiques descriptives des fenêtres conservées ; aucune mesure d'apnée |
| C06 | `optical.rs` | Accord FC firmware ≠ VALIDATED ; statut expérimental, indicateur d'accord séparé | Aucune validation ECG ajoutée |
| C07 | `optical.rs` | Séparation des rafales sur changement `status_raw`, contrôle continuité et présence des indicateurs qualité | Sémantique de l'acceptation firmware toujours à confirmer |
| C08 | `whoop_lab_analysis.rs` | Cache : révision par triggers INSERT/UPDATE/DELETE + profil + offset + décodeur + version ; invalidation de toutes les nuits dépendantes | Conservateur : coût élevé sur historique long, à mesurer avant release |
| C09 | `whoop_lab_analysis.rs` | Ingestion biométrique avec CRC vérifié et curseur/high-watermark dans une transaction | Ne répare pas automatiquement les anciennes lignes déjà ingérées |
| C10 | `health_indices.rs` | Pas de remplissage spectral des trous >5 secondes ni d'extrapolation aux bords | Politique de départ, perd de la couverture ; ECG nécessaire pour optimiser |
| C11 | `whoop_lab_analysis.rs` | FC de repos : 300 secondes réellement consécutives et positives, sinon null | Minimum nocturne reste une définition spécifique, pas FC standardisée |
| C12 | `whoop_lab_analysis.rs` | Conservation des jours manquants dans la série FCmax pour éviter un accès à des jours futurs | Âge/FCmax restent estimés si aucune référence personnelle |
| C13 | `sport.rs`, `Sport.swift`, `train.py` | Modèles candidats inactifs, paramètres validés, promotion explicite atomique, versions immuables | La déclaration de revue n'est pas une signature cryptographique ; contrat de recherche local |
| C14 | `harnet_eval.py` | Filtrage anti-repliement polyphase avant 100→30 Hz, exclusion des transitoires et des segments invalides | Hors ligne ; aucun modèle HAR déployé |
| C15 | `capture_manifest.py` | Validation sidecar de recherche, hashes des fichiers, métadonnées d'horloge/configuration, usage par tête future | Vérifie les métadonnées déclarées et fichiers, pas leur contenu physiologique |
| C16 | tests Rust/Python, workflow | Régressions ciblées et CI | Build iOS et tests matériels restent requis |

Seules les époques complètes sont scorées ; `unscored_boundary_seconds` décrit le reliquat final.
Les durées de sommeil observées sont conservées lorsque des époques manquent, mais marquées
`INCOMPLETE_LOWER_BOUND`. L'efficacité et le score de sommeil sont alors indisponibles. La
récupération nocturne est également neutralisée pour éviter de traiter une interruption comme une
courte nuit réelle. Revoir tous les consommateurs secondaires du JSON et du modèle `LabNight`.

## Ce qui est proposé, pas implémenté

Le plan détaillé couvre : orchestrateur partagé des capteurs, journal des commandes et preuves
d'arrêt, horloges et dérive, IANA/DST, fenêtres de sommeil multiples, réconciliation des doublons,
reconstruction canonique du décodage, cache incrémental optimisé, stockage binaire/Parquet,
chiffrement des exports, SQI multi-usage, pipeline PPG multi-voies et anti-mouvement, PRV sur
horodatage de battements, respiration fusionnée, métriques standardisées, validation prospective.

Ces éléments ne sont pas présentés comme fonctionnels. Les commandes matérielles, filtres et
interfaces ne doivent pas être branchés sans fixtures et tests représentatifs. Leur conception
et leurs critères de réception sont fournis dans `IMPLEMENTATION_PLAN.md` pour qu'Opus puisse
les reprendre sans la conversation initiale.

## Contrats de compatibilité à examiner

- Schéma général `whoop-lab.results.v2` conservé ; ajouts optionnels pour Swift.
- Hypnogramme : U signifie donnée insuffisante, jamais un cinquième stade physiologique.
- `cpc = null` ; `rr_spectral_pattern` est réservé au diagnostic/recherche.
- `respiratory_disturbance.index_0_100 = null`, `odi_available = false`.
- `optical.validated_bursts = 0`, ajout `usable_bursts` et accord avec firmware.
- `spectral_snr` remplacé par `spectral_peak_power_fraction` : vérifier consommateurs externes.
- `sport.install_model` importe inactif par défaut ; `activate=true` nécessite reviewer,
  évaluation held-out déclarée et hash d'artefact. L'app n'active que les fichiers revus.
- Les anciens modèles actifs ne sont pas supprimés automatiquement : les inspecter avant release.
- `lab/analyze.py` est une référence historique v9, **pas un oracle de parité v11**. Ne pas utiliser
  ses résultats inchangés pour invalider les corrections ; une remise en parité est un chantier explicite.

## Commandes de revue

```bash
CARGO_PROFILE_RELEASE_LTO=false CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16 cargo test --manifest-path Rust/core/Cargo.toml --release --locked --lib
python -m pip install -r lab/requirements-review.txt
python -m unittest discover -s lab/tests -v
git diff --check
```

Sur Mac : construire le scheme `GooseSwift` avec le SDK indiqué dans le README, puis tester
une nuit complète, une nuit avec interruption, une synchronisation tardive R26 et un changement
de profil. Le workflow GitHub ne construit pas iOS.

## Décisions attendues d'Opus

- Accepter ou ajuster les politiques de couverture, gaps et abstention ; documenter les raisons.
- Vérifier le budget CPU de `stage_epochs` et l'invalidation globale avant usage quotidien.
- Vérifier que les routes Swift affichent les métriques null sans remplacer par zéro.
- Exécuter une migration/reconstruction contrôlée des anciennes tables biométriques sur copie.
- Prioriser les lots A à F du plan, sans activer un modèle sans tête disponible et validée.
- Garder les données personnelles, captures, bases SQLite et poids hors du dépôt public.

## Références scientifiques et techniques

Les résultats des articles ne sont pas attribués à Goose. Liens consultés durant l'audit :

- Détection PPG, MSPTD/qppg : https://doi.org/10.1088/1361-6579/ac826d
- Lipponen–Tarvainen, artefacts : https://pubmed.ncbi.nlm.nih.gov/31314618/
- CPC réel et limites de correspondance aux stades : https://pubmed.ncbi.nlm.nih.gov/16268385/
- HAR préentraîné : https://pmc.ncbi.nlm.nih.gov/articles/PMC11015005/
- Code HAR : https://github.com/OxWearables/ssl-wearables
- SleepPPG-Net : https://pubmed.ncbi.nlm.nih.gov/36446010/
- SleepPPG-Net2 : https://pubmed.ncbi.nlm.nih.gov/41172609/
- PaPaGei : https://github.com/Nokia-Bell-Labs/papagei-foundation-model
- PPG-DaLiA : https://archive.ics.uci.edu/dataset/495/ppg-dalia
- FC/mouvement/PSG : https://www.physionet.org/content/sleep-accel/1.0.0/
- TROIKA : https://pubmed.ncbi.nlm.nih.gov/25252274/
- Respiration, comparaison expérimentale : https://pubmed.ncbi.nlm.nih.gov/27027672/
- Régularité sommeil : https://pubmed.ncbi.nlm.nih.gov/37738616/
- LF/HF : https://pubmed.ncbi.nlm.nih.gov/23431279/
- Limites VO2 HR ratio : https://pubmed.ncbi.nlm.nih.gov/34495410/
- Limites ACWR : https://pubmed.ncbi.nlm.nih.gov/33332011/


## Réveil intelligent — cahier des charges technique et UI/UX

Ajout du 13 novembre 2026 à la demande de l'utilisateur. **Statut : recommandations à implémenter et à valider, pas fonctionnalités livrées.** Cet ajout est documentaire ; les résultats de tests dans `VALIDATION.md` ne valident pas ces futures adaptations.

Objectif : garantir un réveil fiable à l'heure limite, préserver la durée de sommeil et autoriser une anticipation seulement lorsque les données la justifient. L'estimation des stades et du moment favorable reste expérimentale.

Fichiers à examiner : `Rust/core/src/smart_wake.rs`, `GooseSwift/SmartWake.swift`, `GooseSwift/GooseBLEClient+UserActions.swift`, gestion des commandes/réponses BLE, vues Forme et `docs/SMART_WAKE.md`. Recontrôler les constats sur la branche courante avant modification.

### SW01 — P0 : compatibilité bloquante avec cette PR

Dans `personalise()` et `replay()`, le mapping actuel traite tout caractère autre que W/L/D comme R. Les nouvelles époques U de cette PR seraient donc assimilées au REM.

- Mapper explicitement W/L/D/R ; U, labels absents et caractères invalides restent inconnus.
- Exclure les inconnus des cibles d'apprentissage et métriques de classification.
- Exclure les nuits incomplètes du calcul du besoin et de la dette ; une borne inférieure de durée n'est pas une nuit courte démontrée.
- Propager couverture, provenance et complétude aux consommateurs ; invalider les caches affectés.
- **Critère avant fusion : tests prouvant que U ne devient jamais REM et qu'une interruption n'augmente pas artificiellement la dette.**

### SW02 — P0 : commandes, alarmes et concurrence

Séparer alarme autonome du bracelet, secours système iPhone et adaptation intelligente. La perte de l'adaptation ne doit pas supprimer les secours.

- Identifiants de commande, session et génération ; associer chaque réponse à l'alarme, l'heure et au motif concernés.
- Remplacer la recherche de SUCCESS dans `lastAlarmResponseSummary` par un résultat structuré et corrélé.
- États persistés : requested, acknowledged, failed, expired. Une commande acceptée n'est pas une preuve de vibration physique.
- Priorité arrêt/alarme sur synchronisation historique ; tentatives bornées, expirations visibles et journalisées.
- Ignorer commandes différées et résultats moteur d'une ancienne session, y compris après désactivation puis réarmement.
- Examiner l'annulation individuelle réellement supportée. Ne pas laisser `ramp.stop()`, un test ou une calibration désactiver tous les secours.
- Si seule l'annulation globale existe, documenter la procédure de réarmement confirmée et ses limites ; bloquer les tests incompatibles pendant une session armée.
- Réconcilier les alarmes après reconnexion selon les capacités réelles de lecture du bracelet ; ne pas inventer de confirmation.
- Raccorder le retour d'échec de stimulation au moteur ; ne pas démarrer l'escalade comme si une commande refusée avait été exécutée.

### SW03 — P0 : dates, secours iPhone et reprise

- Distinguer heure récurrente et instant absolu ; afficher `session.windowEnd` pendant une session.
- Un palier tardif ne doit jamais être reporté au lendemain par `nextFutureAlarmDate` ; définir abandon, remplacement ou exécution immédiate.
- Conserver UTC, fuseau IANA et intention utilisateur ; gérer changement de fuseau et DST. Utiliser une horloge monotone pour les délais.
- Intégrer AlarmKit sur iOS compatible, avec contrôle de disponibilité, autorisation et résultat de programmation ; expliciter les limites d'une notification ordinaire sur les autres versions.
- Un timer Swift ou le mode Bluetooth en arrière-plan ne garantit pas une exécution permanente. Utiliser la restauration Core Bluetooth existante.
- Persister session, échéance, commandes confirmées, stimulation demandée et confirmation utilisateur.
- Au relancement, réconcilier avant stimulation ; une session expirée ne se reporte pas automatiquement au lendemain.
- Distinguer panne du moteur adaptatif et échec de programmation des secours.

### SW04 — P0 : réaction, éveil probable et arrêt confirmé

| État | Signification | Effet attendu |
|---|---|---|
| Réaction détectée | Mouvement ou changement physiologique | Observation supplémentaire ; secours conservé |
| Éveil probable | Indices concordants et persistants | Pause éventuelle avec possibilité de relance |
| Arrêt confirmé | Action explicite ou politique automatique séparément validée | Arrêt selon la politique annoncée |

- Une hausse de FC, deux posteriors d'éveil élevés ou un mouvement isolé ne doivent pas supprimer définitivement les secours.
- Le bouton « Je suis réveillé » confirme explicitement ; le double tap a une action configurable et affichée, arrêt ou report.
- Marche soutenue : indice fort à valider, pas preuve universelle ; arrêt automatique configurable et évalué.
- Définir rechute dans l'inactivité, relance et maintien de l'échéance.
- L'arrêt explicite doit fonctionner sans capteurs disponibles.

### SW05 — P1 : compromis durée de sommeil / inertie

Le code compare déjà un coût d'inertie et de sommeil perdu à un coût d'attente. La pénalité de sommeil perdu augmente avec dette et déficit estimés. Les poids profond=1, REM=0,45, léger=0,15 et les coefficients de pénalité restent des choix heuristiques : la littérature ne fournit pas cette conversion universelle.

- Priorité au sommeil lorsque la nuit est courte ou les données incertaines.
- Anticipation maximale choisie par l'utilisateur, y compris zéro ; supprimer l'ouverture obligatoire au moins dix minutes avant l'échéance.
- Un sommeil léger probable ne suffit pas seul : exiger qualité, persistance et marge minimale de bénéfice.
- Séparer coût du sommeil sacrifié, inertie estimée et incertitude ; paramètres documentés, versionnés et inspectables.
- Abstention : données insuffisantes → heure fixe conservée.
- Geler la fenêtre une fois ouverte, sauf modification explicite ou règle documentée.
- Journaliser déclenchements ET décisions d'attendre.
- Remplacer Monte-Carlo par une évaluation déterministe de la même politique lorsque possible ; cela réduit le bruit de calcul sans valider ses hypothèses. Ne pas la qualifier d'optimale sans préciser son cadre.
- Corriger `p_light_or_wake_next_10min` : le maximum des probabilités marginales n'est pas la probabilité de rencontrer au moins une occasion favorable. Calculer une probabilité d'atteinte ou retirer cette sortie UI.

### SW06 — P1 : qualité des données, artefacts et performance

- Fraîcheur et couverture distinctes pour FC, intervalles, IMU et états firmware ; une seule FC récente ne suffit pas.
- Conserver timestamps appareil/réception, source et configuration ; détecter doublons, désordre, trous, dérive et buffers saturés.
- Journaliser les pertes, rejeter valeurs non finies/invalides ; pas d'abandon silencieux.
- Une période sans observations n'est pas du sommeil démontré. Distinguer estimation soutenue par mesures et extrapolation du modèle.
- Endormissement : couverture et persistance requises.
- Remplacer le masque de 70 s par des intervalles de stimulation liés à leur statut, avec marge temporelle.
- Appliquer le marquage aux règles IMU et aux décisions FC/HMM potentiellement perturbées ; conserver ce contexte dans les exports.
- Passer à des caractéristiques glissantes et un filtre incrémental plutôt que recalculer toute la nuit à chaque appel.
- Recalcul borné lors d'arrivées tardives ; vérifier l'équivalence avec le calcul complet.

### SW07 — P1 : modèle, personnalisation et planificateur

- Garder le HMM expérimental ; supprimer « calibré par construction ».
- Séparer posterior, qualité des données et calibration mesurée.
- L'hypnogramme heuristique n'est pas une référence indépendante. Encadrer ou désactiver par défaut l'auto-personnalisation sur pseudo-labels ; version de référence et retour arrière disponibles.
- Valider taille, valeurs et normalisation des vecteurs avant indexation.
- Planificateur : durée cible ajustable avec provenance et incertitude. Le p75 des nuits observées n'est pas automatiquement le besoin physiologique.
- Retirer par défaut l'ajout de minutes fondé sur le déficit de profond estimé ; ajustements activité/dette présentés comme heuristiques.
- Respecter les jours calendaires réels et les données manquantes ; intégrer les siestes seulement si suffisamment fiables.
- Rejeu : borner toutes les données et feedbacks à l'instant simulé, y compris planificateur et personnalisation.
- Clés de cache : données, paramètres, version et borne temporelle.
- Aucun nouveau modèle appris sans poids, tête et prétraitement directement exploitables. Pour une future tête de réponse au réveil, préparer uniquement captures/labels et documenter modèle envisagé, cible et validation requise.

### SW08 — P1 : stimulation haptique

- Préférer des motifs autonomes validés sur le bracelet à une dépendance aux envois BLE toutes les cinq secondes.
- Vérifier coexistence, chevauchement et annulation des alarmes.
- Définir refus, retard, déconnexion, arrêt et escalade ; plafond de stimulation explicite.
- Profils simples doux/standard/soutenu, durées et progression paramétrées.
- Afficher des niveaux de commande, pas une amplitude physique en pourcentage non mesurée.
- Arrêt explicite accessible immédiatement.
- Distinguer motif accepté, vibration observée et efficacité de réveil.

### SW09 — P1 : gestion partagée des capteurs

- Gestionnaire commun au réveil, sport et captures de recherche ; acquisition/libération des besoins par consommateur.
- Ne pas couper un flux encore utilisé ailleurs.
- Distinguer demande, commande acceptée et données reçues.
- Conserver l'activation tardive FC/IMU lorsqu'elle protège batterie et configuration optique ; mesurer avant de modifier les fréquences.
- Nettoyage après crash/session expirée.
- Séparer expériences optiques nocturnes et réveil quotidien.

### SW10 — UI/UX : écran principal et réglages

Trois questions : quand, est-ce programmé, que puis-je modifier ?

- Heure principale : « Demain · 07:00 », issue de la session active.
- Fenêtre : « Réveil possible entre 06:40 et 07:00 ».
- États séparés : programmation bracelet, secours iPhone, adaptation intelligente.
- Modes « Heure fixe » et « Fenêtre flexible » ; anticipation maximale, profil haptique, répétition par jours, double tap et report.
- Modification d'une session active via reprogrammation contrôlée ; ne pas annoncer le nouvel horaire comme confirmé avant acceptation.
- Réutiliser Forme, Dynamic Type, VoiceOver, grandes cibles et contrastes nocturnes ; ne pas coder les états uniquement par couleur.

### SW11 — UI/UX : messages exacts et progression

Afficher les états vérifiables :

- « Programmation en cours » / « Confirmé sur le bracelet » / « Échec de programmation — réessayer ».
- « Adaptation disponible » / « Données insuffisantes — heure fixe conservée ».
- « Connexion interrompue — alarme bracelet précédemment confirmée ».
- « Annulation en cours » / « Annulation non confirmée ».

Ne pas masquer une erreur par un retour immédiat à Inactif. Remplacer « meilleur moment » par « adaptation dans ta fenêtre » ; qualifier le besoin estimé. Réserver les pourcentages de stades et détails du modèle à une vue avancée.

### SW12 — UI/UX : réveil et retour matinal

- Pendant la vibration : écran sobre, grand bouton « Je suis réveillé », report facultatif avec nouvel horaire explicite, statut du secours.
- Ne pas afficher Réveillé sur simple réaction.
- Après : première stimulation, confirmation, minutes d'anticipation, relances, raison du déclenchement/de l'attente et limites des données.
- Feedback bref horodaté : somnolence, intensité adaptée/trop faible/trop forte, préférence pour un réveil plus tard, arrêt erroné ou réveil manqué.
- Seconde mesure de somnolence facultative quelques minutes après ; ne pas modifier automatiquement la politique à partir de quelques réponses.

### SW13 — UI/UX : laboratoire séparé

Déplacer tests optiques, calibration, captures, sondes de mouvement, posteriors HMM et journaux techniques vers Laboratoire. Bloquer les opérations incompatibles avec une session active et expliquer pourquoi. Le parcours quotidien ne doit pas nécessiter de comprendre le BLE.

### SW14 — Tests et critères de réception

Tests logiciels à ajouter :

- U jamais REM ; nuit incomplète n'augmente pas artificiellement la dette ; aucun accès au futur en rejeu.
- Réponses BLE et résultats moteur d'anciennes sessions ignorés.
- Commande tardive jamais reportée au lendemain ; affichage d'échéance stable après son passage.
- Arrêt explicite sans capteurs ; réaction seule ne supprimant pas le secours.
- Refus, timeout, déconnexion, redémarrage, autorisation refusée, DST et changement de fuseau.
- Données absentes/tardives, artefacts de vibration et perte de buffers.
- Équivalence incrémental/complet ; opérations de test n'altérant pas l'alarme limite.

Tests matériels séparés : alarme sans téléphone, coexistence/annulation, app suspendue/arrêtée, délais des commandes et double tap, batterie, continuité des flux, motifs réellement produits. Les tests synthétiques ne valident ni le matériel ni la physiologie.

### SW15 — Évaluation et ordre de livraison

Comparer prospectivement heure fixe et fenêtre flexible selon un protocole défini à l'avance. Mesurer réveils manqués/retards, arrêts erronés, minutes sacrifiées, délai de confirmation, relances, somnolence et batterie. Une comparaison à ses propres pseudo-labels ne démontre pas le bénéfice.

Rejouer les délais d'arrivée, interruptions et indisponibilités observables à l'époque ; comparer aussi à une politique simple.

Ordre : (1) P0 et compatibilité U, (2) décision et qualité, (3) UI/UX, (4) optimisation et évaluation. Pour chaque lot, livrer comportement, tests exécutés et limites. Actualiser les contradictions historiques de `docs/SMART_WAKE.md`. Conserver la PR brouillon et ne pas fusionner automatiquement.

### Références complémentaires du réveil

Ces références motivent les précautions ; elles ne valident ni les coefficients du moteur ni l'efficacité du réveil Goose.

- Inertie et contexte circadien/stade : https://pubmed.ncbi.nlm.nih.gov/18729646/
- Synthèse sur l'inertie : https://pubmed.ncbi.nlm.nih.gov/27692973/
- AlarmKit, programmation et autorisation : https://developer.apple.com/documentation/AlarmKit/scheduling-an-alarm-with-alarmkit
- Notifications, Concentration et mode silencieux : https://developer.apple.com/design/human-interface-guidelines/managing-notifications
- Limites de l'exécution Bluetooth en arrière-plan : https://developer.apple.com/library/archive/documentation/NetworkingInternetWeb/Conceptual/CoreBluetooth_concepts/CoreBluetoothBackgroundProcessingForIOSApps/PerformingTasksWhileYourAppIsInTheBackground.html

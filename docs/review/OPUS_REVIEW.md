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
5. Valider sur une **copie** de base réelle et sur iPhone ; les tests synthétiques ne valident pas la physiologie.

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

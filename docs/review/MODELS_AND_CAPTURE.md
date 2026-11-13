# Modèles : exploitabilité réelle et capture pour têtes futures

## Règle de décision

**Aucun nouveau modèle appris n'est activé dans cette proposition.** L'existence de poids d'un
encodeur ne signifie pas qu'une tête prédisant les paramètres souhaités est disponible. Un article
avec un bon kappa ne suffit pas non plus à certifier la compatibilité WHOOP.

Un modèle devient « directement exploitable » seulement si : dépôt et commit exacts, hash des
poids, tête exacte, classes/unités, fréquence et prétraitement documentés, exemple d'inférence
reproduit, test sur captures WHOOP et politique d'abstention sont tous présents. Vérifier aussi
la licence des poids et du code lors du packaging. Une fiche sans ces preuves reste en capture seule.

| Candidat | Statut pour Goose | Capture à préparer | Tête / cible future | Référence requise |
|---|---|---|---|---|
| HARNet-10, OxWearables | Encodeur exploitable dans le banc d'essai existant ; aucune tête validée course/tennis/musculation WHOOP | R21 six axes 100 Hz, orientation/port, saturation, FC, bouts négatifs, début/fin et transitions | Classification activité ; segmentation temporelle séparée | Annotation humaine et vidéo synchronisée |
| HARNet + branche gyro | Architecture proposée uniquement ; pas un checkpoint prêt à l'emploi | Même acquisition, pas de réduction destructive du gyro | Fusion multimodale, classe inconnu, éventuellement distillation | Test participant indépendant, puis séances futures personnelles |
| Encodeur IMU personnel | Entraînement requis, pas de modèle préconisé prêt à reconnaître tous les exercices | Séries complètes + transitions + exercices silencieux au poignet, répétitions manuelles | Reconnaissance d'exercice par métrique/prototypes et tête comptage distincte | Vidéo, série/exercice, reps, repos, côté porté |
| SleepPPG-Net / Net2 | Référence de recherche ; checkpoint/head/prétraitement non reproduits dans ce travail | R20 continu toute nuit, R21 si faisable, R18, configuration optique et gaps | Adaptation d'une tête 4 classes W/N1+N2/N3/REM au poignet ; pas d'inférence embarquée proposée | PSG alignée 30 s ; EEG revu si contexte pilote, clairement distingué |
| PaPaGei | Encodeur, pas un fournisseur automatique de toutes les constantes physiologiques | R20 brut continu, canaux séparés, DC, états LED/AFE, IMU synchronisée | Tête SQI spécifique HR/PRV/morphologie | Annotation qualité experte + ECG pour les usages cardiaques |
| PaPaGei respiration | Hypothèse de transfert à comparer à une fusion déterministe | R20, amplitude/DC, battements, IMU, respiration référence | Régression fréquence respiratoire + incertitude | Ceinture respiratoire/capnographie synchronisée |
| AnyPPG / Pulse-PPG | Non retenus comme solutions directes dans cette branche : compatibilité et tête non vérifiées | Le stockage PPG brut proposé conserve les possibilités futures | Aucune tête promise avant audit reproductible du checkpoint exact | Dépendra de la tâche explicitement choisie |

MSPTD/qppg, FFT, Welch et filtres adaptatifs sont des **algorithmes**, pas des modèles à tête
à entraîner. Ils peuvent être évalués directement, mais leur intégration au pipeline WHOOP reste
conditionnée à des tests de continuité, unités, timing et qualité. Aucun portage prétendument fidèle
n'est inclus sans comparaison à la référence.

## Sidecar de capture implémenté

`lab/research/capture_manifest.py` valide les métadonnées et calcule les hashes des fichiers déjà
exportés. Il ne commande pas le bracelet, ne décode pas les trames, ne vérifie pas le contenu des
colonnes et ne fabrique pas de labels. Il est utilisable maintenant sur les fichiers existants
complétés par un sidecar. Les adaptations de l'export Swift/Rust restent dans le lot C du plan.

```bash
python lab/research/capture_manifest.py /chemin/session/capture.json \
  --output /chemin/session/manifest.json
```

Exemple de structure (remplacer par les valeurs **observées**, ne pas recopier des valeurs supposées) :

```json
{
  "schema": "goose.research.capture.v1",
  "participant_id": "pseudonyme",
  "session_id": "session-unique",
  "device_pseudonym": "device-local",
  "firmware": "version-observee",
  "decoder_version": "version-effective",
  "wear_location": "left_wrist",
  "timezone": "Europe/Brussels",
  "clock_alignment": {
    "method": "sync_markers",
    "offset_seconds": 0.0,
    "drift_ppm": 0.0,
    "uncertainty_ms": 20.0
  },
  "intended_tasks": ["har_harnet_head"],
  "streams": [{
    "path": "imu.csv",
    "modality": "accel",
    "sample_rate_hz": 100.0,
    "rate_source": "measured",
    "units": "g",
    "timestamp_column": "device_time",
    "quality_mask_column": "valid",
    "configuration_id": "imu-config-1",
    "gaps_verified": true,
    "gaps": []
  }],
  "labels": [{"source": "human_activity", "reference": true}]
}
```

Pour une voie PPG : ajouter `wavelength` (peut être `unknown`), `led_source`, `photodiode`,
`gain`, `offset` (null si non résolus). Changer `configuration_id` lorsqu'un réglage change.
Le manifest conserve tous les autres champs fournis : protocole, chemins de labels, hash vidéo,
observateur, horodatage de correction, source de référence, biais de capteur et paramètres de filtrage.

La sortie indique **toujours** `deployment_ready: false`. Elle signale les modalités manquantes
et la présence déclarée d'une référence compatible. `gaps_verified` est une attestation de
l'opérateur/exporteur, pas une inspection automatique des séries.

## Labels et partitionnement

- L'état firmware et les prédictions antérieures sont des features/pseudo-labels, pas une référence.
- Conserver label prédit, label humain et label de référence dans des colonnes distinctes.
- Ne pas déduire automatiquement un début de séance exact d'un simple bouton « confirmer ».
- Stocker les intervalles temporels avec incertitude, annotateur et version d'annotation.
- Split par personne pour généralisation ; split chronologique par séance pour personnalisation.
- Une personne, une nuit ou une séance ne doit pas fuir dans plusieurs splits via fenêtres chevauchantes.
- Normalisation, filtrage appris et calibration des probabilités ne voient que les splits autorisés.
- Fixer une version du jeu de données et de la liste des participants par split avant entraînement.
- Conserver les négatifs réalistes : conduite, cuisine, marche rapide, escaliers, gestes répétitifs sans sport.

## Promotion d'un modèle personnel existant

`lab/sport/train.py` écrit désormais dans `candidates/`, jamais `models/`. Son LOSO reste
exploratoire et n'autorise pas une promotion. Après test tenu à l'écart et revue :

1. Archiver l'évaluation (matrice de confusion, macro-F1, taux de couverture, fausses séances,
   période du test, groupes, hash des données et version moteur).
2. Ajouter `metrics.review_status = "approved"`, `reviewed_by`,
   `held_out_evaluation` et `evaluation_artifact_sha256`.
3. Copier le fichier revu vers `models/` pour le chemin existant de l'application, ou appeler
   `sport.install_model` avec `activate=true`.
4. Vérifier la réponse `ok` avant de mémoriser la version installée.
5. Pour rollback, réactiver une ancienne version revue via la même opération transactionnelle.

La validation Rust vérifie dimensions, ordre des features/classes, valeurs finies et échelles
positives. Les versions sont immuables. Ces contrôles ne remplacent pas l'inspection de l'artefact
d'évaluation par le reviewer : les métriques JSON restent des déclarations locales.

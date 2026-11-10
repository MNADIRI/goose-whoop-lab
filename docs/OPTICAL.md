# Optique WHOOP 5.0 : ce qu'on reçoit et ce qu'on peut en tirer

Mesures du 10 novembre (firmware 50.38.1) : 59 rafales R26 de la nuit, et 1 513 s de R20 lors du test optique
(749 s en direct et 764 s en historique, récupérées dans l'export iCloud).

## Flux disponibles

| Flux | Activation | Contenu | Couverture |
|---|---|---|---|
| R26 (paquet 47) | par défaut | 1 voie, 25 échantillons par seconde, longueur d'onde inconnue | rafales de 40 s toutes les 20 min environ (3–4 % de la nuit) |
| R20 historique (paquet 47) | `TOGGLE_PERSISTENT_R20` (153) | 5 blocs × 2 voies, 25 échantillons par seconde et par voie active, réglages LED et détecteur | continu tant que c'est activé |
| R20 direct (paquet 43) | `ENABLE_OPTICAL_DATA` (107) + `TOGGLE_OPTICAL_MODE` (108) | même format, 1 trame par seconde, environ 1 s de retard | continu tant que c'est activé |

Rôle des blocs observé sur le segment immobile (127 s, FC du bracelet 53 bpm) :

| Bloc | LED (source / courant) | Détecteur | AC/DC | Pouls | Lecture |
|---|---|---|---|---|---|
| 0 A / B | 1 / 3 350 et 4 / 6 700 | 3 / 4 | 0,72 % | 52,7 bpm, part du pic 0,89 | même longueur d'onde (r = 0,98, rapport des rapports 0,98), forte perfusion : vert probable |
| 4 A / B | 2 / 200 | 3 / 1 | 0,04 / 0,16 % | 52,7 bpm | autre LED, faible part pulsatile : rouge ou infrarouge probable |
| 3 A / B | 2 / 0 et 4 / 0 | 3 / 4 | — | aucun | LED éteinte : lumière ambiante / bruit de fond (~250) |
| 1, 2 | 3 / 12 750 et 2 / 5 300 | 1 | — | — | configurés mais 0 échantillon dans ce mode |

Les longueurs d'onde ne sont **pas étiquetées** dans les trames. « Vert » et « rouge ou infrarouge » sont des déductions
tirées des niveaux de perfusion, pas des faits.

R26 a un gain automatique : le niveau continu passe de 60 000 à 470 000 d'une rafale à l'autre, en même temps que
`status_raw` (2816, 2656, 2832). Sa valeur de base est **signée** : un bug de décodage est corrigé (décodeur v2).

## Faisabilité

| Mesure | Verdict | Raison |
|---|---|---|
| **SpO₂** | **Non, pour l'instant** | Il faut deux longueurs d'onde rouge **et** infrarouge pulsatiles au même endroit, et un étalonnage. On voit une seule LED faiblement pulsatile (source 2). La troisième source (3, courant 12 750), candidate infrarouge, est configurée mais n'est pas échantillonnée dans ce mode. Un rapport vert/rouge n'est pas une méthode SpO₂ établie. |
| **Pression systolique / diastolique** | **Non** | Pas d'ECG ni de second site (donc pas de temps de transit du pouls), 25 Hz trop lent pour la forme du pouls, et pas de brassard pour étalonner. Une valeur serait trompeuse. |
| **Fréquence respiratoire** | **Déjà meilleure par les intervalles R-R** | Sur des rafales R26 de 40 s, on trouve 8 à 26 respirations/min. La méthode R-R sur toute la nuit donne 15,5, sur 148 fenêtres. R20 en continu permettrait de la recouper, sans gain prouvé. |
| **Pouls, qualité du signal** | **Oui (fait)** | R26 : 0,6 bpm d'écart moyen avec la FC du bracelet. R20 : 52,7 contre 53 bpm. |
| **Indice de perfusion relatif** | **Oui, expérimental** | AC/DC du bloc 0, comparable seulement à réglage LED identique. |
| **Lumière ambiante / bracelet mal porté** | **Oui** | Bloc 3, pour contrôler la qualité. |

## Modèles PPG de base (AnyPPG, PaPaGei)

Ce sont de vrais modèles ouverts :
- **PaPaGei** (Nokia Bell Labs, ICLR 2025) : 57 000 h de PPG publics, 20 tâches évaluées.
- **AnyPPG** (2025) : appris en s'appuyant sur l'ECG, environ 100 000 h ; plage de 25 à 128 Hz selon une source secondaire.

**Pas maintenant**, pour quatre raisons :
1. Ils ne créent pas d'information absente : sans voie infrarouge, pas de SpO₂ ; sans référence, pas de pression.
2. Ils attendent des segments continus. Seul R20 en continu (activé à la demande) s'en approche, et un R20 continu toute la
   nuit pèse 2 140 o/s, soit environ 30 Mo pour 4 h, avec un coût batterie inconnu.
3. On n'a aucune référence (oxymètre, brassard, polysomnographie) pour valider ce qu'ils prédiraient.
4. Il faudrait embarquer et faire tourner un réseau de neurones sur l'iPhone pour un gain non démontré.

Usage raisonnable plus tard : calculer des représentations du signal (embeddings) sur du R20 vert continu pour des modèles
personnels (stades de sommeil, récupération). C'est à évaluer contre les mesures actuelles, avec des données de référence.

## Prochaine étape utile

Activer R20 en historique pendant 20 à 30 min **de sommeil**. WHOOP calcule sa SpO₂ la nuit, et les blocs 1 et 2 (source 3)
pourraient n'être échantillonnés qu'à ce moment-là. S'ils contiennent une voie pulsatile, une étude SpO₂ relative (ratio of
ratios, tendances nocturnes, sans valeur absolue tant qu'il n'y a pas d'étalonnage) deviendrait envisageable.

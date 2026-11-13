# Validation de la proposition

Date : 2026-11-13. Base : `b5ac3b48712ed8c173c75497223de7ea89a03ae2`.

## Résultats locaux

- Rust 1.94.0, Linux, profil release : **77 tests réussis, 0 échec, 10 ignorés** (87 découverts ; 64,93 s hors compilation).
- Python : **7 tests réussis**, manifestes de capture et prétraitement HAR.
- `git diff --check` : aucun problème de whitespace.

Commande Rust :

```bash
CARGO_PROFILE_RELEASE_LTO=false CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16 cargo test --manifest-path Rust/core/Cargo.toml --release --locked --lib
```

Commande Python :

```bash
python -m pip install -r lab/requirements-review.txt
python -m unittest discover -s lab/tests -v
```

Le workflow ajouté reprend ces commandes. Les résultats ci-dessus sont locaux ; ils ne constituent pas un résultat GitHub Actions.

## Limites à lever avant fusion

- Les 10 tests ignorés par la suite existante ne sont pas comptés comme réussis.
- Aucun build iOS/Xcode ni test BLE/WHOOP matériel exécuté dans cet environnement Linux.
- Aucune validation sur base personnelle, ECG, PSG ou référence respiratoire indépendante.
- Les tests synthétiques vérifient les comportements logiciels, pas une précision clinique.
- La reconstruction des anciennes lignes biométriques et le coût du cache sur un historique long restent à tester sur copie.
- Aucun nouveau modèle, checkpoint ni tête entraînée n’a été exécuté ou validé.

Voir `OPUS_REVIEW.md` pour les contrats modifiés, et `IMPLEMENTATION_PLAN.md` pour les chantiers uniquement proposés.

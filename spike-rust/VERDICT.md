# Spike Rust — ratatui + crossterm, taille du binaire et souris

## Comment lancer

```bash
cargo run --release
```

Ou avec le binaire compilé :

```bash
./target/release/kmc-spike-rust.exe
```

Attendu : une liste de cinq cases (`alpha`, `bravo`, `charlie`, `delta`, `echo`),
la ligne du curseur surlignée, en plein écran.

- Clic sur une ligne : coche/décoche et déplace le curseur.
- Molette : déplace le curseur.
- Flèches / `j` `k` : déplacent le curseur.
- Espace ou Entrée : coche/décoche la ligne courante.
- `q` ou Ctrl+C : quitte.

Comportement identique aux spikes OpenTUI (`spike/spike.tsx`) et Go
(`spike-go/main.go`), pour comparaison directe.

## Stack

- [ratatui](https://ratatui.rs) 0.30.2
- [crossterm](https://github.com/crossterm-rs/crossterm) 0.29.0, utilisé via la
  réexportation `ratatui::crossterm` pour garantir que les types d'événements
  correspondent à ceux du backend
- `ratatui::init()` (mode raw + écran alternatif + hook de panique) puis
  `EnableMouseCapture`

## Vérifié automatiquement

- `cargo clippy --release -- -D warnings` : passe, aucun avertissement.
- `cargo fmt --check` : passe.
- `cargo build --release` : compile, binaire `PE32+ x86-64`.

## Taille du binaire — résultat principal

| Binaire | Taille | Contenu embarqué |
|---|---|---|
| `kmc.exe` (Bun + OpenTUI, actuel) | 112 Mo | runtime Bun + moteur natif OpenTUI |
| `spike-go.exe` (Go + Bubble Tea) | 3,4 Mo | binaire Go statique + runtime Go |
| `kmc-spike-rust.exe` (Rust + ratatui) | **350 Ko** | binaire statique, pas de runtime |

Profil release avec `strip = true`, l'équivalent du `-ldflags="-s -w"` utilisé
côté Go. Aucune option agressive (pas de LTO, pas d'`opt-level = "z"`, pas de
`panic = "abort"`) : il reste donc de la marge si la taille devenait critique.

Écarts : ~327x plus petit que le binaire Bun actuel, ~10x plus petit que
l'équivalent Go. La différence avec Go vient de l'absence de runtime et de
ramasse-miettes embarqués.

## Souris

Aucune des trois causes rencontrées côté Go ne se manifeste ici, et ce n'est pas
un hasard : crossterm traite les deux points délicats dans sa couche Windows.

**Transitions par bouton plutôt que XOR global.** Côté Go, Bubble Tea déduit le
bouton avec `btn := p ^ s` puis un `switch` sur des valeurs de bouton uniques.
Le terminal intégré de Kiro signalant le bouton droit comme enfoncé pendant les
déplacements, un clic gauche donnait `0x03`, valeur non couverte, donc
`MouseButtonNone` et clic perdu. crossterm teste chaque bouton séparément, le
gauche en premier (`src/event/sys/windows/parse.rs`) :

```rust
if button_state.left_button() && !buttons_pressed.left {
    Some(MouseEventKind::Down(MouseButton::Left))
} else if !button_state.left_button() && buttons_pressed.left {
    Some(MouseEventKind::Up(MouseButton::Left))
} else if button_state.right_button() && !buttons_pressed.right {
    // ...
```

Un état parasite sur un autre bouton ne masque donc pas le clic gauche.

**Coordonnées relatives à la fenêtre.** Bubble Tea transmet la coordonnée brute
du buffer console (`ev.Y = int(e.MousePositon.Y)`), ce qui décale les clics dès
qu'il y a de l'historique au-dessus. crossterm fait la conversion lui-même :

```rust
let window_size = ScreenBuffer::current()?.info()?.terminal_window();
Ok(y - window_size.top)
```

L'écran alternatif reste utilisé ici (`ratatui::init()` l'active par défaut,
comme OpenTUI), mais les coordonnées seraient correctes même sans lui.

**Press/release.** `MouseEventKind` distingue explicitement `Down`, `Up`,
`Drag`, `Moved`, `ScrollUp`, `ScrollDown` : pas d'ambiguïté à filtrer côté
application. En revanche le clavier a le même piège que partout sur Windows —
`KeyEventKind::Press` et `Release` sont tous deux émis, d'où le filtre explicite
dans `run()`.

### À tester

- [ ] Terminal intégré de Kiro : clic coche la bonne ligne, molette déplace le
      curseur
- [ ] Windows Terminal / standalone : idem, un clic coche une seule fois

La lecture du code de crossterm laisse attendre que les deux cas fonctionnent
sans correctif, mais seul le test le confirme.

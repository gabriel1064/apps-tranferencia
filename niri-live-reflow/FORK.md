# niri-live-reflow

Fork local do [niri](https://github.com/YaLTeR/niri) com *live-reflow* durante o arrasto interativo de janelas tiled (comportamento no estilo Hyprland).

- **Base:** `v26.04` (`8ed0da44`) — a mesma versão do niri 26.04 que o Gabriel está a correr
- **Branch:** `feature/live-reflow-drag`
- **Upstream:** `https://github.com/YaLTeR/niri.git`

## O que mudou

No niri original, `Mod`+arrasto (ou arrasto pela titlebar) **destaca** a janela do layout: as outras fecham o buraco e só aparece a sombra de *insert-hint* até largares o rato.

Neste fork, com `layout.interactive-move.live-reflow true` (predefinição):

1. A janela arrastada continua a seguir o cursor (overlay `Moving`).
2. **Em cima / em baixo** (empilhar na coluna): a janela-alvo **divide ~50/50** — encolhe e abre o vão do tamanho em que a arrastada encaixaria. Não empurra a janela inteira para baixo.
3. **Meio / lado** (coluna nova): as outras **deslizam para o lado** e abrem um vão com a largura da janela arrastada.
4. **Sem sombra** de insert-hint enquanto o live-reflow está activo.
5. Largar o rato só confirma o sítio já visível. `Escape` restaura a origem.
6. **Mod+botão direito (resize):** escolhe sempre o **canto mais perto** do cursor (nunca o centro “morto”), para redimensionar em **qualquer direção**.
7. **Sem buraco:** janela sozinha numa coluna preenche sempre a altura útil (Fixed height curto é ignorado).

## Configuração

No `config.kdl`:

```kdl
layout {
    interactive-move {
        live-reflow true
    }
}
```

- `true` (default neste fork): live-reflow ligado
- `false`: comportamento stock do niri (só insert-hint)

**Não ponhas `interactive-move` no `~/.config/niri/config.kdl` enquanto o compositor da sessão for o niri da distro.** O niri 26.04 oficial não conhece esse nó e recusa o config. O fork usa live-reflow mesmo sem essa secção (default `true`). Só adiciona o bloco quando a sessão estiver a correr `niri-live-reflow`.

## Window chrome (SSD Super island)

Segurar **Super/Mod** deforma a **borda SSD** no canto superior direito de cada janela: uma “ilha” na mesma cor da borda cresce a partir do canto (recorte) e mostra 5 bolinhas.

| Bolinha | Ação |
|--------|------|
| Vermelha | Fechar |
| Amarela | Minimizar (workspace longe) |
| Ciano | 50% largura |
| Verde | Maximize column (100% viewport) |
| Roxa | Fullscreen |

Soltar Super anima o fecho. Isto **não** é overlay Quickshell — é render do compositor no mesmo caminho da borda (`src/layout/window_chrome.rs`).

O overlay `niri-window-chrome` / `niri-super-chrome` deve estar **desligado** (conflita).

Para usar: a sessão tem de correr este fork (`niri-switch-live-reflow` ou `~/.local/bin/niri-live-reflow --session`).

## Window chrome (Super + SSD island)

Holding **Super (Mod)** deforms the window **SSD border** itself: a solid island in the
same border color grows from the top-right corner into the window (cut reveal) with
five traffic-light controls:

| Ball | Action |
|------|--------|
| Red | Close |
| Yellow | Minimize (park on workspace 32) |
| Cyan | 50% column width |
| Green | Maximize column (100% viewport) |
| Purple | Real fullscreen |

This is painted in `Tile::render` after the border stroke — not a Quickshell overlay.
The old `niri-window-chrome` / `niri-super-chrome` user units must stay disabled.

Requires this fork (stock Arch `niri` has no chrome). Switch with:

```bash
~/.local/bin/niri-switch-live-reflow
```

## Compilar

```bash
cd ~/src/niri-live-reflow
# Sem screencast (não precisa de clang). Suficiente para testar o live-reflow.
cargo build --release --no-default-features --features dbus,systemd
```

O binário fica em `target/release/niri`.

Para o build **completo** (screencast xdp-gnome, igual ao pacote Arch) falta o pacote `clang` (bindgen/pipewire). Sem password de sudo nesta sessão não deu para instalar. Depois:

```bash
sudo pacman -S clang
cargo build --release
```

## Instalar sem substituir o niri da distro

O wrapper `~/.local/bin/niri-live-reflow` aponta para o binário deste fork. O `/usr/bin/niri` do pacote Arch **não é alterado**.

### Experimentar aninhado (recomendado)

Dentro da sessão niri atual, num terminal:

```bash
~/.local/bin/niri-live-reflow
```

Isto abre um niri *nested* como janela Wayland. Testa o arrasto tiled aí. Ctrl+C no terminal fecha.

Podes passar um config temporário:

```bash
~/.local/bin/niri-live-reflow --config ~/.config/niri/config.kdl
```

### Trocar o compositor da sessão

Só depois de verificares o nested. Opções:

1. **PATH:** garante que `~/.local/bin` vem **antes** de `/usr/bin` no PATH do display manager / `environment.d`, e que a sessão chama `niri-live-reflow` em vez de `niri`.
2. **Drop-in systemd (user):** se a sessão usa `niri.service`, um override `ExecStart=` para o wrapper. Não faças symlink por cima de `/usr/bin/niri`.

Para voltar atrás, usa de novo `/usr/bin/niri` (pacote `niri 26.04-1`).

## Rebase para tags novas do upstream

```bash
cd ~/src/niri-live-reflow
git fetch upstream --tags
git checkout feature/live-reflow-drag
# exemplo: rebase em cima da tag que estás a correr
git rebase v26.04
# ou, quando quiseres saltar para uma tag mais nova:
# git rebase v25.xx
```

Conflitos esperados (ficheiros tocados de propósito):

- `niri-config/src/layout.rs` — campo `interactive-move`
- `src/layout/scrolling.rs` — placeholder / `insert_position`
- `src/layout/mod.rs` — `InteractiveMoveState`, motion/drop/cancel
- `src/layout/workspace.rs` — wrappers
- `src/input/mod.rs` / `src/input/move_grab.rs` — Escape cancela o move

Não faças `git push --force` nem alteres `git config` de utilizador.

Remotes:

- `upstream` → GitHub oficial
- `origin` → local (`/home/gabriel/src/niri-live-reflow`)

## Como funciona no código

- O tile arrastado continua **fora** do layout (`InteractiveMoveState::Moving`), renderizado no cursor.
- `Layout::sync_live_reflow()` corre no motion e no `update_insert_hint`.
- `InsertPosition::NewColumn`: vão **lateral** com a largura da janela arrastada (as outras colunas deslizam).
- `InsertPosition::InColumn`: as tiles existentes são redimensionadas como se houvesse +1 tile auto (split ~50/50) e o vão vertical usa a altura restante. Sem empurrar a janela-alvo inteira para baixo.
- Insert-hint **não** é desenhado enquanto o live-reflow tiled está activo.
- No drop, o vão é removido e a janela real é inserida no mesmo slot.
- No cancel, a inserção usa o `origin` gravado no pickup.

Ficheiros tocados (mínimo para rebase):

- `niri-config/src/layout.rs`, `niri-config/src/lib.rs`
- `src/layout/scrolling.rs`, `src/layout/workspace.rs`, `src/layout/mod.rs`, `src/layout/tests.rs`
- `src/input/mod.rs`, `src/input/move_grab.rs`
- `FORK.md`

## Limitações conhecidas (MVP)

- **Overview:** sem live-reflow; insert-hint stock entre workspaces.
- **Floating:** sem live-reflow (a janela já segue o cursor no espaço floating).
- **Colunas tabbed:** inserção *in-column* no modo tabbed não faz split vertical (as tabs não empilham em Y). Coluna nova ao lado abre vão lateral.
- **Multi-monitor:** o vão live-reflow é no workspace **sob o cursor**. Arrastar para outro output funciona; o vão no output anterior fecha.
- **Workspaces novos (NewAt):** inserção numa workspace ainda não criada (overview / bordas) usa insert-hint, não placeholder.
- Não é uma porta do Hyprland em C++; o layout é nativo em Rust, no código de scrolling do niri.

## Testes

```bash
cd ~/src/niri-live-reflow
cargo test -p niri-config parse_interactive_move_live_reflow
cargo test --lib interactive_move_live_reflow
```

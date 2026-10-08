# apps-tranferencia

Código-fonte do meu ambiente niri, para recompilar e reinstalar noutro sistema.

| Pasta | Conteúdo |
|---|---|
| `niri-live-reflow/` | Fork do [niri](https://github.com/YaLTeR/niri) (base `v26.04`, `8ed0da44`), branch `feature/live-reflow-drag`. Ver `niri-live-reflow/FORK.md`. |
| `rail/waybar/` | Waybar: `config-niri.jsonc`, `style-niri.css`, assets e scripts (`scripts/niri-rail.py`, `niri-layout.py`, etc.). |
| `noctalia/` | Configuração do Noctalia (pacote oficial `noctalia` 5.1.0, sem alterações ao código): `settings.wanted.toml`, plugin `clean-desktop`, scripts de arranque. |
| `niri-config/` | `~/.config/niri/config.kdl` |
| `bin/` | Scripts de `~/.local/bin` usados pela sessão. |
| `systemd-user/` | Units de `~/.config/systemd/user` (inclui o override que faz a sessão usar o fork). |
| `nemo-layer/` | `PKGBUILD` do Nemo com gtk-layer-shell (ícones no desktop). |

Os caminhos nos configs assumem o utilizador `gabriel` (`/home/gabriel/...`). Noutro utilizador, substitui-os.

## Compilar o niri

Dependências (Arch): `rust cargo clang pkgconf libxkbcommon libinput mesa seatd pipewire pango cairo libdisplay-info wayland`.

```bash
cd niri-live-reflow
cargo build --release
install -Dm755 target/release/niri ~/.local/bin/niri-live-reflow
```

## Instalar o resto

```bash
mkdir -p ~/.config/niri ~/.config/waybar ~/.config/noctalia ~/.local/share/noctalia ~/.local/bin ~/.config/systemd/user
cp niri-config/config.kdl ~/.config/niri/
cp -r rail/waybar/. ~/.config/waybar/
cp noctalia/config/settings.wanted.toml ~/.config/noctalia/
cp -r noctalia/plugins ~/.local/share/noctalia/
install -m755 noctalia/bin/* bin/* ~/.local/bin/
cp -r systemd-user/. ~/.config/systemd/user/
systemctl --user daemon-reload
```

Pacotes externos usados: `noctalia`, `waybar`, `python-gobject`, `gtk-layer-shell`, `polkit-gnome`, `playerctl`, `brightnessctl`, `alacritty`, `fuzzel`.

O script `bin/waybar` aponta para um build local em `~/.local/opt/waybar-root`; sem ele, apaga esse wrapper e usa o `waybar` da distro.

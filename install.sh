#!/usr/bin/env bash
set -euo pipefail

# lily-dashboard Arch Linux Installer
# Sets up dependencies, builds release binary, configures GTK dark defaults,
# and wires Hyprland (Hyprlua or classic hyprland.conf) out of the box.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
AUTO_YES=0
SKIP_PKGS=0
SKIP_BUILD=0
SKIP_HYPR=0

usage() {
    cat <<'EOF'
Usage: ./install.sh [OPTIONS]

Arch Linux setup script for lily-dashboard.

Options:
  -y, --yes       Non-interactive mode (accept prompts automatically)
  --no-pkgs       Skip pacman dependency installation
  --no-build      Skip cargo build --release
  --no-hypr       Skip Hyprland config setup
  -h, --help      Show this help message
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        -y|--yes)    AUTO_YES=1; shift ;;
        --no-pkgs)   SKIP_PKGS=1; shift ;;
        --no-build)  SKIP_BUILD=1; shift ;;
        --no-hypr)   SKIP_HYPR=1; shift ;;
        -h|--help)   usage; exit 0 ;;
        *)           printf '[ERR] Unknown option: %s\n' "$1" >&2; usage; exit 1 ;;
    esac
done

log_step() { printf '\n==> %s\n' "$1"; }
log_ok()   { printf '  [OK] %s\n' "$1"; }
log_warn() { printf '  [!]  %s\n' "$1"; }
log_err()  { printf '  [ERR] %s\n' "$1" >&2; }

if [[ "${EUID}" -eq 0 ]]; then
    log_err "Do not run ./install.sh as root. It calls sudo only when installing pacman packages."
    exit 1
fi

if ! command -v pacman >/dev/null 2>&1; then
    log_err "pacman was not found. This installer targets Arch Linux (and Arch derivatives)."
    exit 1
fi

ask_yes_no() {
    local prompt="$1"
    if [[ "${AUTO_YES}" -eq 1 ]]; then
        return 0
    fi
    local ans=""
    read -r -p "  ${prompt} [Y/n]: " ans || true
    case "${ans,,}" in
        ""|y|yes) return 0 ;;
        *)        return 1 ;;
    esac
}

ensure_ini_setting() {
    local ini_file="$1"
    local key="$2"
    local val="$3"
    mkdir -p "$(dirname "${ini_file}")"
    if [[ ! -f "${ini_file}" ]]; then
        printf '[Settings]\n%s=%s\n' "${key}" "${val}" > "${ini_file}"
        return
    fi
    if ! grep -q '^[[:space:]]*\[Settings\]' "${ini_file}"; then
        local tmp_ini
        tmp_ini="$(mktemp)"
        {
            printf '[Settings]\n'
            cat "${ini_file}"
        } > "${tmp_ini}"
        mv "${tmp_ini}" "${ini_file}"
    fi
    if grep -qE "^[[:space:]]*${key}[[:space:]]*=" "${ini_file}"; then
        sed -i -E "s|^[[:space:]]*${key}[[:space:]]*=.*|${key}=${val}|" "${ini_file}"
    else
        printf '%s=%s\n' "${key}" "${val}" >> "${ini_file}"
    fi
}

# 1. Packages
if [[ "${SKIP_PKGS}" -eq 0 ]]; then
    log_step "Installing core build & runtime packages"
    CORE_PKGS=(base-devel pkgconf gtk4 gtk4-layer-shell ttf-meslo-nerd)
    if ! command -v cargo >/dev/null 2>&1 && ! command -v rustup >/dev/null 2>&1; then
        CORE_PKGS+=(rust)
    fi
    sudo pacman -S --needed --noconfirm "${CORE_PKGS[@]}"
    log_ok "Core build dependencies ready"

    log_step "Recommended runtime tools"
    printf '  Includes: clipboard (wl-clipboard), wallpaper (swww), screenshots (grim, slurp, hyprpicker),\n'
    printf '  notifications (libnotify, swaync), media (playerctl, mpv, yt-dlp, pipewire-pulse),\n'
    printf '  network/bluetooth/storage/cloud/icons (networkmanager, bluez-utils, udisks2, rclone, papirus-icon-theme).\n'
    if ask_yes_no "Install available recommended packages via pacman?"; then
        CANDIDATE_OPT=(
            wl-clipboard swww grim slurp hyprpicker libnotify swaync
            playerctl mpv yt-dlp pipewire-pulse networkmanager bluez-utils
            udisks2 rclone papirus-icon-theme
        )
        AVAILABLE_OPT=()
        SKIPPED_OPT=()
        for pkg in "${CANDIDATE_OPT[@]}"; do
            if pacman -Si "${pkg}" >/dev/null 2>&1; then
                AVAILABLE_OPT+=("${pkg}")
            else
                SKIPPED_OPT+=("${pkg}")
            fi
        done
        if [[ "${#AVAILABLE_OPT[@]}" -gt 0 ]]; then
            sudo pacman -S --needed --noconfirm "${AVAILABLE_OPT[@]}"
            log_ok "Installed ${#AVAILABLE_OPT[@]} recommended packages"
        fi
        if [[ "${#SKIPPED_OPT[@]}" -gt 0 ]]; then
            log_warn "Not in official pacman sync repos (install via AUR if needed): ${SKIPPED_OPT[*]}"
        fi
    else
        log_warn "Skipped optional runtime tools"
    fi
else
    log_step "Skipping package installation (--no-pkgs)"
fi

# 2. Build & Symlink
BIN_PATH="${SCRIPT_DIR}/target/release/lily-dashboard"
if [[ "${SKIP_BUILD}" -eq 0 ]]; then
    log_step "Building lily-dashboard (release)"
    if ! command -v cargo >/dev/null 2>&1; then
        log_err "cargo not found in PATH. Install rust/rustup and re-run."
        exit 1
    fi
    (cd "${SCRIPT_DIR}" && cargo build --release)
    log_ok "Built ${BIN_PATH}"
else
    log_step "Skipping cargo build (--no-build)"
fi

mkdir -p "${HOME}/.local/bin"
if [[ -x "${BIN_PATH}" ]]; then
    ln -sf "${BIN_PATH}" "${HOME}/.local/bin/lily-dashboard"
    log_ok "Linked ~/.local/bin/lily-dashboard -> ${BIN_PATH}"
else
    log_warn "Release binary not found at ${BIN_PATH}; Hyprland config will still point to ${BIN_PATH}"
fi

# 3. Default Folders & GTK Dark Preferences
log_step "Setting up user folders & GTK dark defaults"
mkdir -p "${HOME}/Pictures/Wallpapers" "${HOME}/Documents/plans" "${HOME}/.config/lily-dashboard"
log_ok "Ensured ~/Pictures/Wallpapers and ~/Documents/plans"

ensure_ini_setting "${HOME}/.config/gtk-3.0/settings.ini" "gtk-application-prefer-dark-theme" "1"
ensure_ini_setting "${HOME}/.config/gtk-4.0/settings.ini" "gtk-application-prefer-dark-theme" "1"
log_ok "Enabled gtk-application-prefer-dark-theme=1 in gtk-3.0 & gtk-4.0 settings.ini"

if command -v gsettings >/dev/null 2>&1; then
    cur_scheme="$(gsettings get org.gnome.desktop.interface color-scheme 2>/dev/null || echo "'default'")"
    if [[ "${cur_scheme}" == "'default'" || -z "${cur_scheme}" ]]; then
        gsettings set org.gnome.desktop.interface color-scheme 'prefer-dark' >/dev/null 2>&1 || true
        log_ok "Set GNOME/GTK portal color-scheme to prefer-dark"
    fi
fi

# 4. Hyprland Integration
if [[ "${SKIP_HYPR}" -eq 0 ]]; then
    log_step "Configuring Hyprland integration"
    HYPR_DIR="${HOME}/.config/hypr"
    mkdir -p "${HYPR_DIR}"

    if [[ ! -f "${HYPR_DIR}/hyprland.lua" && ! -f "${HYPR_DIR}/hyprland.conf" ]]; then
        if [[ -f /usr/share/hypr/hyprland.lua ]]; then
            cp /usr/share/hypr/hyprland.lua "${HYPR_DIR}/hyprland.lua"
            log_ok "Seeded ~/.config/hypr/hyprland.lua from /usr/share/hypr/hyprland.lua"
        elif [[ -f /usr/share/hypr/hyprland.conf ]]; then
            cp /usr/share/hypr/hyprland.conf "${HYPR_DIR}/hyprland.conf"
            log_ok "Seeded ~/.config/hypr/hyprland.conf from /usr/share/hypr/hyprland.conf"
        else
            cat > "${HYPR_DIR}/hyprland.conf" <<'EOF'
# Minimal Hyprland configuration created by lily-dashboard installer
monitor = ,preferred,auto,1
EOF
            log_warn "No system Hyprland template found; created minimal ~/.config/hypr/hyprland.conf"
        fi
    fi

    if [[ -f "${HYPR_DIR}/hyprland.lua" ]]; then
        cat > "${HYPR_DIR}/lily-dashboard.lua" <<EOF
-- Managed by lily-dashboard/install.sh (safe to re-run or customize)
hl.on("hyprland.start", function()
    hl.exec_cmd("dbus-update-activation-environment --systemd WAYLAND_DISPLAY XDG_CURRENT_DESKTOP=Hyprland")
    hl.exec_cmd("sleep 0.2 && ${BIN_PATH}")
end)

-- Tap SUPER to toggle lily-dashboard
hl.bind("SUPER + Super_L", hl.dsp.exec_cmd("${BIN_PATH}"), { release = true })

-- Frosted glass blur for lily-dashboard surfaces
hl.layer_rule({ name = "lily-dashboard-blur",     match = { namespace = "^lily-dashboard$" },     blur = true, blur_popups = true, ignore_alpha = 0.25 })
hl.layer_rule({ name = "lily-media-popout-blur", match = { namespace = "^lily-media-popout$" }, blur = true, ignore_alpha = 0.25 })
hl.layer_rule({ name = "lily-sticky-blur",        match = { namespace = "^lily-sticky$" },        blur = true, ignore_alpha = 0.25 })
EOF
        if ! grep -qE 'require\(["'\'']lily-dashboard["'\'']\)' "${HYPR_DIR}/hyprland.lua"; then
            printf '\nrequire("lily-dashboard")\n' >> "${HYPR_DIR}/hyprland.lua"
            log_ok "Added require(\"lily-dashboard\") to ~/.config/hypr/hyprland.lua"
        fi
        log_ok "Wrote Hyprlua module ~/.config/hypr/lily-dashboard.lua"
    else
        cat > "${HYPR_DIR}/lily-dashboard.conf" <<EOF
# Managed by lily-dashboard/install.sh (safe to re-run or customize)
exec-once = dbus-update-activation-environment --systemd WAYLAND_DISPLAY XDG_CURRENT_DESKTOP=Hyprland
exec-once = sleep 0.2 && ${BIN_PATH}

# Tap SUPER to toggle lily-dashboard
bindr = SUPER, Super_L, exec, ${BIN_PATH}

# Frosted glass blur for lily-dashboard surfaces
layerrule = blur, ^lily-dashboard$
layerrule = ignorezero, ^lily-dashboard$
layerrule = blur, ^lily-media-popout$
layerrule = ignorezero, ^lily-media-popout$
layerrule = blur, ^lily-sticky$
layerrule = ignorezero, ^lily-sticky$
EOF
        if ! grep -qF "lily-dashboard.conf" "${HYPR_DIR}/hyprland.conf"; then
            printf '\nsource = ~/.config/hypr/lily-dashboard.conf\n' >> "${HYPR_DIR}/hyprland.conf"
            log_ok "Added source = ~/.config/hypr/lily-dashboard.conf to ~/.config/hypr/hyprland.conf"
        fi
        log_ok "Wrote Hyprland snippet ~/.config/hypr/lily-dashboard.conf"
    fi

    if [[ -n "${HYPRLAND_INSTANCE_SIGNATURE:-}" ]] && command -v hyprctl >/dev/null 2>&1; then
        hyprctl reload >/dev/null 2>&1 || true
        log_ok "Reloaded running Hyprland configuration"
    fi
else
    log_step "Skipping Hyprland configuration (--no-hypr)"
fi

log_step "Installation complete"
printf '  - Launch now:    %s &\n' "${BIN_PATH}"
printf '  - Toggle bind:   Tap SUPER (Super_L) or run `lily-dashboard --toggle`\n'
if [[ ":${PATH}:" != *":${HOME}/.local/bin:"* ]]; then
    printf '  - Note:          Add ~/.local/bin to your shell PATH to run `lily-dashboard` directly.\n'
fi

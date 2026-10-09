# 1. Environment & System Variables
export EDITOR="nvim"
export MAKEFLAGS="-j$(sysctl -n hw.logicalcpu)"

if [[ -z "$HOMEBREW_PREFIX" ]]; then
    if [[ -d "/opt/homebrew" ]]; then
        HOMEBREW_PREFIX="/opt/homebrew"
    elif [[ -d "/usr/local" ]]; then
        HOMEBREW_PREFIX="/usr/local"
    else
        HOMEBREW_PREFIX="$(brew --prefix 2>/dev/null)"
    fi
fi

setopt interactive_comments


# 2. PATH Configurations
export PATH="$HOME/.local/bin:$PATH"
export PATH="$HOME/Library/Android/sdk/platform-tools:$PATH"
export PATH="$(go env GOPATH)/bin:$PATH"
export PATH="$HOME/Developer/github/kartinul/dotfiles/scripts:$PATH"
export PATH="/Library/Frameworks/Python.framework/Versions/3.14/bin:$PATH"
export BUN_INSTALL="$HOME/.bun"
export PATH="$BUN_INSTALL/bin:$PATH"
export PNPM_HOME="/Users/kartinul/Library/pnpm"
case ":$PATH:" in
  *":$PNPM_HOME/bin:"*) ;;
  *) export PATH="$PNPM_HOME/bin:$PATH" ;;
esac
export PATH="/Users/kartinul/Developer/github/kartinul/savecodex/target/release:$PATH"
export PATH="/Users/kartinul/.antigravity-ide/antigravity-ide/bin:$PATH"
if [ -f '/Users/kartinul/Developer/github/forks/google-cloud-sdk/path.zsh.inc' ]; then
    . '/Users/kartinul/Developer/github/forks/google-cloud-sdk/path.zsh.inc'
fi

# 3. Tool & Service Environment Loaders
[[ -f "$HOME/.local/bin/env" ]] && . "$HOME/.local/bin/env"
[[ -f "/Users/kartinul/.unity/env" ]] && . "/Users/kartinul/.unity/env"
[[ -f "$HOME/.railway/env" ]] && source "$HOME/.railway/env"

# 4. Application Configurations
# Savecodex
export CODE_PROVIDER=gemini
export CODE_MODEL=gemini-3.6-flash,gemini-3.5-flash,gemini-3-flash-preview,gemini-2.5-flash
export INPUT_PROVIDER=gemini
export INPUT_MODEL=gemini-3.5-flash-lite,gemini-3.1-flash-lite
export SAVECODEX_STYLE=macos
export SAVECODEX_DOC_TEXT="\nKartik Sharma - 992501030333 - {}\n"
export SAVECODEX_OUTPUT="992501030333_{}.docx"


# git
export SSH_AUTH_SOCK=~/Library/Containers/com.maxgoedjen.Secretive.SecretAgent/Data/socket.ssh

# 5. Aliases
alias ls='eza --group-directories-first --icons=auto'
alias lt='eza --tree --level=3  --icons=auto'
alias scamtute='/Users/kartinul/Developer/github/kartinul/dotfiles/scripts/scamtute.py --grayscale --brightness 1.8 --threshold 215'
alias clang++20="clang++ -std=c++20"
alias gcc="gcc-16"
alias g++="g++-16"
alias py="python3"
alias python="python3"
alias pip="pip3"
alias v="nvim"

alias pblog='url=$(pbpaste 2>/dev/null | curl -s --data-binary @- https://paste.rs 2>/dev/null); printf "%s\n" "$url"; printf "%s" "$url" | pbcopy'
alias pbrmlog='curl -s -X DELETE "$(pbpaste)"'

portkill() {
    kill -9 $(lsof -t -i:"$1")
}

notify() {
    osascript -e "display notification \"$*\" with title \"notify\""
}

alias sshg="gcloud compute ssh hermes-agent"
alias tstart="tmux new-session -d -s zrok-ssh 'zrok access private $ZROK_PRIVATE --bind 127.0.0.1:9191' && echo 'Tunnel Started.'"
alias tstop="tmux kill-session -t zrok-ssh 2>/dev/null && echo 'Tunnel stopped.'"

alias ybstart="yabai --start-service && skhd --start-service"
alias ybstop="yabai --stop-service && skhd --stop-service"
alias ybreload="ybstop && ybstart"

fuckoff() {
  xattr -d com.apple.quarantine "$1" 2>/dev/null
  open "$1"
}

# 6. ZSH Plugins & Keybindings
# Custom widgets
vi-yank-pbcopy() {
    zle vi-yank
    echo -n "$CUTBUFFER" | pbcopy
}
zle -N vi-yank-pbcopy

# Tab completion / autosuggest-accept widget
_tab_complete_or_accept() {
    local current_word="${LBUFFER##*[[:space:]]}"

    if [[ "$current_word" == */* || "$current_word" == .* || "$current_word" == ~* ]]; then
        zle expand-or-complete
    elif [[ -n "$POSTDISPLAY" ]]; then
        zle autosuggest-accept
    else
        zle expand-or-complete
    fi
}
zle -N _tab_complete_or_accept

# zsh-vi-mode hooks (register custom bindings & clipboard integration)
function zvm_after_init() {
    bindkey '^I' _tab_complete_or_accept
    bindkey -M vicmd 'y' vi-yank-pbcopy
}

function zvm_after_select_vi_mode() {
    case $KEYMAP in
        vicmd|visual) echo -n "$CUTBUFFER" | pbcopy ;;
    esac
}

# zsh-vi-mode
ZVM_INIT_MODE=sourcing
if [[ -f "$HOMEBREW_PREFIX/opt/zsh-vi-mode/share/zsh-vi-mode/zsh-vi-mode.plugin.zsh" ]]; then
    source "$HOMEBREW_PREFIX/opt/zsh-vi-mode/share/zsh-vi-mode/zsh-vi-mode.plugin.zsh"
fi

# Autosuggestions
if [[ -f "$HOMEBREW_PREFIX/share/zsh-autosuggestions/zsh-autosuggestions.zsh" ]]; then
    source "$HOMEBREW_PREFIX/share/zsh-autosuggestions/zsh-autosuggestions.zsh"
fi

# 7. Shell Prompts & Navigation Tools
eval "$(starship init zsh)"
eval "$(zoxide init zsh --cmd j)"

# 8. Completions
if [[ -d "$HOMEBREW_PREFIX/share/zsh/site-functions" ]]; then
    fpath=("$HOMEBREW_PREFIX/share/zsh/site-functions" $fpath)
fi

autoload -Uz compinit
() {
    setopt local_options extended_glob
    local zcompdump="${ZDOTDIR:-$HOME}/.zcompdump"
    if [[ -n "$zcompdump"(#qN.mh+24) ]] || [[ ! -f "$zcompdump" ]]; then
        compinit
    else
        compinit -C
    fi
}

if [ -f '/Users/kartinul/Developer/github/forks/google-cloud-sdk/completion.zsh.inc' ]; then
    . '/Users/kartinul/Developer/github/forks/google-cloud-sdk/completion.zsh.inc'
fi
[ -s "/Users/kartinul/.bun/_bun" ] && source "/Users/kartinul/.bun/_bun"

# 9. Syntax Highlighting
if [[ -f "$HOMEBREW_PREFIX/share/zsh-syntax-highlighting/zsh-syntax-highlighting.zsh" ]]; then
    source "$HOMEBREW_PREFIX/share/zsh-syntax-highlighting/zsh-syntax-highlighting.zsh"
fi

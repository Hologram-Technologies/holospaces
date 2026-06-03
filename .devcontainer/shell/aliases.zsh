# Ari-flavored, container-safe aliases for holospaces development.

# General
alias g='git'
alias te='tree'
alias cleanup="find . -type f -name '*.DS_Store' -ls -delete"
alias peek='tee >(cat 1>&2)'
alias lc='tokei'
alias _='sudo'
alias le='less -R'
alias r='rg'
alias eo='echo'
alias so='source'
alias ka='echo'
alias rl='curl'
alias ua='unalias'
alias m='mkdir'
alias ma='man'
alias dus='du -s'
alias to='touch'
alias q='exit'
alias mx='chmod +x'
alias rr='rm -rf'
alias tp='type'
alias ba='bash'
alias sz='exec zsh'

if command -v eza >/dev/null 2>&1; then
  alias a='eza'
  alias aa='eza -la'
  alias tre='eza -Ta'
else
  alias a='ls'
  alias aa='ls -la'
  alias tre='tree -a'
fi

if command -v bat >/dev/null 2>&1; then
  alias t='bat'
elif command -v batcat >/dev/null 2>&1; then
  alias t='batcat'
fi

# Easier navigation
alias ..='cd ..'
alias ...='cd ../..'
alias ....='cd ../../..'

# Git / GitHub
alias ph='git push origin HEAD'
alias pf='git push --force'
alias prb='git pull --rebase'
alias gs='git status -sb'
alias gst='git status'
alias gd='git diff'
alias gdc='git diff --cached'
alias gl='git log --graph --pretty=format:"%Cred%h%Creset -%C(yellow)%d%Creset %s %Cgreen(%cr) %C(bold blue)<%an>%Creset" --abbrev-commit'

# Python
alias p='python3'
alias yi='python3 -i'

# Docker
alias d='docker'
alias cu='docker compose'
alias cup='docker compose up'
alias docker_clean_images='docker rmi $(docker images -a -q)'
alias docker_clean_ps='docker rm $(docker ps --filter=status=exited --filter=status=created -q)'

# Kubernetes
alias k='kubectl'
alias kl='kubectl logs'
alias kf='kubectl logs -f'
alias kde='kubectl describe'
alias ke='kubectl explain'
alias kg='kubectl get'
alias ks='kubectl get pods'
alias kd='kubectl delete pods'
alias ksw='kubectl get pods -o=wide -w'
alias kp='kubectl port-forward'

# Node / package managers
alias no='node'
alias it='npm init'
alias ig='npm install -g'
alias iis='npm install && npm start'
alias ia='npm add'
alias ir='npm run'
alias ire='npm remove'
alias is='npm start'
alias dev='npm run dev'
alias igl='npm list -g --depth 0'
alias y='yarn'
alias ya='yarn add'
alias yr='yarn run'
alias yre='yarn remove'
alias ys='yarn start'
alias ydev='yarn run dev'
alias pn='pnpm'
alias pni='pnpm install'

# Rust / Go / Ruby
alias ru='rustup'
alias o='go'
alias or='go run'
alias oo='go install'
alias ov='go vet'
alias ogu='go get -u'
alias ob='go build'
alias rb='ruby'

# Piping
alias h2='head -n 2'
alias h10='head -n 10'
alias t10='tail -n 10'

# Chezmoi / Hugo
alias cz='chezmoi'
alias us='hugo server -D'
alias ut='hugo server -w'
alias u='hugo'

# Utility
alias net="ping ya.ru | grep -E --only-match --color=never '[0-9.]+ ms'"
alias history-stat="history 0 | awk '{print \$2}' | sort | uniq -c | sort -n -r | head"

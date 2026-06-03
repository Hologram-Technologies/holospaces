FROM mcr.microsoft.com/devcontainers/base:ubuntu-24.04

ARG USERNAME=vscode
ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        bat \
        build-essential \
        ca-certificates \
        cmake \
        curl \
        eza \
        gh \
        git \
        git-lfs \
        graphviz \
        just \
        less \
        openjdk-21-jdk \
        pkg-config \
        qemu-system-misc \
        qemu-utils \
        ripgrep \
        tree \
        vim \
        wget \
        zsh \
    && rm -rf /var/lib/apt/lists/*

RUN if [ ! -d "/home/${USERNAME}/.oh-my-zsh" ]; then \
        git clone --depth=1 https://github.com/ohmyzsh/ohmyzsh.git "/home/${USERNAME}/.oh-my-zsh"; \
    fi \
    && chsh -s /usr/bin/zsh "${USERNAME}" \
    && mkdir -p "/home/${USERNAME}/.config/git" \
    && chown -R "${USERNAME}:${USERNAME}" "/home/${USERNAME}/.oh-my-zsh" "/home/${USERNAME}/.config"

COPY --chown=${USERNAME}:${USERNAME} shell/zshrc /home/${USERNAME}/.zshrc
COPY --chown=${USERNAME}:${USERNAME} shell/aliases.zsh /home/${USERNAME}/.config/holospaces-aliases.zsh
COPY --chown=${USERNAME}:${USERNAME} shell/gitconfig /home/${USERNAME}/.config/git/holospaces.gitconfig
COPY --chown=${USERNAME}:${USERNAME} shell/gitconfig-home /home/${USERNAME}/.gitconfig

FROM rust:slim

# Install Node.js (LTS) and system deps
RUN apt-get update && apt-get install -y \
    curl \
    git \
    && curl -fsSL https://deb.nodesource.com/setup_22.x | bash - \
    && apt-get install -y nodejs \
    && rm -rf /var/lib/apt/lists/*

# Install Claude Code
RUN npm install -g @anthropic-ai/claude-code

# Install br (beads issue tracker)
RUN cargo install beads_rust --bin br

WORKDIR /project

ENTRYPOINT ["claude"]

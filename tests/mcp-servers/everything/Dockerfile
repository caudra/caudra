FROM node:26.12.1-bookworm-slim
LABEL caudra-mcp-it=""
RUN apt-get update \
    && apt-get upgrade --yes --no-install-recommends \
    && rm -rf /var/lib/apt/lists/*
RUN npm install --global npm@12.2.1 \
    && npm install --global @modelcontextprotocol/server-everything@2.0.1 \
    && npm cache clean --force
ENTRYPOINT ["mcp-server-everything", "stdio"]

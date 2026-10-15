FROM node:26.12.0-bookworm-slim
LABEL caudra-mcp-it=""
RUN npm install --global npm@12.2.0 \
    && npm install --global @modelcontextprotocol/server-everything@1.0.0 \
    && npm cache clean --force
ENTRYPOINT ["mcp-server-everything", "stdio"]

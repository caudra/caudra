FROM ghcr.io/home-assistant/home-assistant:2026.10.1
LABEL caudra-mcp-it=""
RUN apk upgrade --no-cache
COPY configuration.yaml /config/configuration.yaml
COPY onboard.py /caudra/onboard.py

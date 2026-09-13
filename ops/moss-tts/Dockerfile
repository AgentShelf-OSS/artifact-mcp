FROM python:3.11.14-slim-bookworm@sha256:65a93d69fa75478d554f4ad27c85c1e69fa184956261b4301ebaf6dbb0a3543d
ENV PYTHONDONTWRITEBYTECODE=1 PYTHONUNBUFFERED=1 HF_HOME=/app/vendor/models/.cache/huggingface HF_HUB_DISABLE_TELEMETRY=1
RUN pip install --no-cache-dir numpy==2.2.6 scipy==1.15.2 onnxruntime==1.20.1 sentencepiece==0.2.0 soundfile==0.13.1 huggingface_hub==0.30.2
WORKDIR /app
COPY vendor/ /app/vendor/
COPY server.py /app/
ENV PYTHONPATH=/app:/app/vendor
USER 65532:65532
EXPOSE 8788
CMD ["python", "-u", "/app/server.py"]

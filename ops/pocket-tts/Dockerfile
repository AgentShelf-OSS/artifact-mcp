FROM python:3.11.14-slim-bookworm@sha256:65a93d69fa75478d554f4ad27c85c1e69fa184956261b4301ebaf6dbb0a3543d
RUN pip install --no-cache-dir torch==2.8.0 --index-url https://download.pytorch.org/whl/cpu
# The timestamped fork keeps the Pocket 3.1.0 model API and adds streamed
# WordStart/WordEnd events. Pin the trial commit so protocol output is stable.
RUN pip install --no-cache-dir "pocket-tts-timestamped @ https://github.com/dpm63/pocket-tts-timestamped/archive/65037e84c1885e7faa3e482b89fe3c304e2dada2.tar.gz" soundfile==0.13.1
WORKDIR /app
COPY server.py stream_control.py timed_protocol.py cache_store.py /app/
USER 65532:65532
EXPOSE 8788
CMD ["python", "-u", "/app/server.py"]

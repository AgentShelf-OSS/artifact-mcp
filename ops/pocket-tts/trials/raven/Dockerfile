FROM python:3.11.14-slim-bookworm@sha256:65a93d69fa75478d554f4ad27c85c1e69fa184956261b4301ebaf6dbb0a3543d
RUN apt-get update && apt-get install -y --no-install-recommends build-essential git curl ca-certificates libsndfile1 && rm -rf /var/lib/apt/lists/*
RUN pip install --no-cache-dir cmake==3.31.6 numpy==2.4.6 onnx==1.20.1 onnxruntime==1.23.2 soundfile==0.13.1
WORKDIR /raven
COPY source/ /raven/
# CMake cannot resolve the pinned older dr_libs commit from a shallow clone.
RUN python -c 'from pathlib import Path; p=Path("CMakeLists.txt"); s=p.read_text(); old="GIT_TAG cd99e2cca5ccb48f8d000f1cd844424dcdbbaf59 # dr_libs pinned\n    GIT_SHALLOW TRUE"; assert old in s; p.write_text(s.replace(old,old.replace("GIT_SHALLOW TRUE","GIT_SHALLOW FALSE")))'
RUN cmake -B .build -DCMAKE_BUILD_TYPE=Release -DBUILD_SHARED_LIB=ON && cmake --build .build -j2
# Use the pinned dependencies above rather than resolving a new uv environment
# for every graph rewrite. The model inputs remain upstream hash-verified.
RUN python -c 'from pathlib import Path; p=Path("tools/prepare_models.sh"); s=p.read_text(); old="PY_ONNX=(uv run --no-project --with onnx --with onnxruntime --with numpy python)"; assert old in s; p.write_text(s.replace(old,"PY_ONNX=(python)"))'
ENV LD_LIBRARY_PATH=/raven OMP_NUM_THREADS=1 OPENBLAS_NUM_THREADS=1
USER 65532:65532
CMD ["./pocket-tts", "--help"]

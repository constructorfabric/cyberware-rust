{{/*
toolkit-common: standard Deployment.

Supports both the platform-host (host mode, runs the whole in-process stack) and
individual OoP gears (each in its own pod, registering with the platform-host's
DirectoryService). Key Profile 3 wiring:

  * Probes hit the OoP HTTP surface: /healthz (liveness) and /readyz (readiness).
  * SA-token projection (values.saToken.enabled) mounts a projected
    ServiceAccount token with audience `toolkit-internal` at
    /var/run/secrets/tokens/toolkit-internal for platform-plane auth
    (TokenReview validation on the receiving gear).
  * The gear config ConfigMap is mounted read-only at values.config.mountPath.
  * values.directoryEndpoint (OoP gears) is injected as TOOLKIT_DIRECTORY_ENDPOINT.
*/}}
{{- define "toolkit-common.deployment" -}}
apiVersion: apps/v1
kind: Deployment
metadata:
  name: {{ include "toolkit-common.fullname" . }}
  labels:
    {{- include "toolkit-common.labels" . | nindent 4 }}
spec:
  replicas: {{ .Values.replicaCount }}
  selector:
    matchLabels:
      {{- include "toolkit-common.selectorLabels" . | nindent 6 }}
  template:
    metadata:
      labels:
        {{- include "toolkit-common.selectorLabels" . | nindent 8 }}
        {{- with .Values.podLabels }}
        {{- toYaml . | nindent 8 }}
        {{- end }}
      annotations:
        {{- if .Values.config.content }}
        checksum/config: {{ include "toolkit-common.configmap" . | sha256sum }}
        {{- end }}
        {{- with .Values.podAnnotations }}
        {{- toYaml . | nindent 8 }}
        {{- end }}
    spec:
      serviceAccountName: {{ include "toolkit-common.serviceAccountName" . }}
      {{- with .Values.imagePullSecrets }}
      imagePullSecrets:
        {{- toYaml . | nindent 8 }}
      {{- end }}
      {{- with .Values.podSecurityContext }}
      securityContext:
        {{- toYaml . | nindent 8 }}
      {{- end }}
      containers:
        - name: {{ .Chart.Name }}
          image: "{{ .Values.image.repository }}:{{ .Values.image.tag | default .Chart.AppVersion }}"
          imagePullPolicy: {{ .Values.image.pullPolicy }}
          {{- with .Values.command }}
          command:
            {{- toYaml . | nindent 12 }}
          {{- end }}
          {{- with .Values.args }}
          args:
            {{- toYaml . | nindent 12 }}
          {{- end }}
          {{- with .Values.containerSecurityContext }}
          securityContext:
            {{- toYaml . | nindent 12 }}
          {{- end }}
          ports:
            - name: http
              containerPort: {{ .Values.service.port }}
              protocol: TCP
            {{- if .Values.grpc.enabled }}
            - name: grpc
              containerPort: {{ .Values.grpc.port }}
              protocol: TCP
            {{- end }}
          env:
            - name: POD_NAME
              valueFrom:
                fieldRef:
                  fieldPath: metadata.name
            - name: POD_NAMESPACE
              valueFrom:
                fieldRef:
                  fieldPath: metadata.namespace
            {{- if .Values.directoryEndpoint }}
            - name: TOOLKIT_DIRECTORY_ENDPOINT
              value: {{ .Values.directoryEndpoint | quote }}
            {{- end }}
            {{- with .Values.extraEnv }}
            {{- toYaml . | nindent 12 }}
            {{- end }}
          volumeMounts:
            {{- if .Values.config.content }}
            - name: config
              mountPath: {{ .Values.config.mountPath }}
              subPath: {{ .Values.config.fileName }}
              readOnly: true
            {{- end }}
            {{- if .Values.saToken.enabled }}
            - name: toolkit-internal-token
              mountPath: {{ .Values.saToken.mountPath }}
              readOnly: true
            {{- end }}
            {{- with .Values.extraVolumeMounts }}
            {{- toYaml . | nindent 12 }}
            {{- end }}
          livenessProbe:
            httpGet:
              path: {{ .Values.probes.liveness.path }}
              port: http
            initialDelaySeconds: {{ .Values.probes.liveness.initialDelaySeconds }}
            periodSeconds: {{ .Values.probes.liveness.periodSeconds }}
            failureThreshold: {{ .Values.probes.liveness.failureThreshold }}
          readinessProbe:
            httpGet:
              path: {{ .Values.probes.readiness.path }}
              port: http
            initialDelaySeconds: {{ .Values.probes.readiness.initialDelaySeconds }}
            periodSeconds: {{ .Values.probes.readiness.periodSeconds }}
            failureThreshold: {{ .Values.probes.readiness.failureThreshold }}
          resources:
            {{- toYaml .Values.resources | nindent 12 }}
      volumes:
        {{- if .Values.config.content }}
        - name: config
          configMap:
            name: {{ include "toolkit-common.fullname" . }}
        {{- end }}
        {{- if .Values.saToken.enabled }}
        - name: toolkit-internal-token
          projected:
            sources:
              - serviceAccountToken:
                  path: {{ .Values.saToken.fileName }}
                  audience: {{ .Values.saToken.audience }}
                  expirationSeconds: {{ .Values.saToken.expirationSeconds }}
        {{- end }}
        {{- with .Values.extraVolumes }}
        {{- toYaml . | nindent 8 }}
        {{- end }}
      {{- with .Values.nodeSelector }}
      nodeSelector:
        {{- toYaml . | nindent 8 }}
      {{- end }}
      {{- with .Values.affinity }}
      affinity:
        {{- toYaml . | nindent 8 }}
      {{- end }}
      {{- with .Values.tolerations }}
      tolerations:
        {{- toYaml . | nindent 8 }}
      {{- end }}
{{- end -}}

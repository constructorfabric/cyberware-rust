{{/*
toolkit-common: ServiceAccount.

Rendered only when serviceAccount.create is true. A dedicated ServiceAccount is
required for SA-token projection (platform-plane auth via TokenReview): the
projected token's identity is this account.
*/}}
{{- define "toolkit-common.serviceAccount" -}}
{{- if .Values.serviceAccount.create -}}
apiVersion: v1
kind: ServiceAccount
metadata:
  name: {{ include "toolkit-common.serviceAccountName" . }}
  labels:
    {{- include "toolkit-common.labels" . | nindent 4 }}
  {{- with .Values.serviceAccount.annotations }}
  annotations:
    {{- toYaml . | nindent 4 }}
  {{- end }}
automountServiceAccountToken: {{ .Values.serviceAccount.automount | default true }}
{{- end -}}
{{- end -}}

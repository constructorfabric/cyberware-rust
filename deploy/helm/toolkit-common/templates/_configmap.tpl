{{/*
toolkit-common: ConfigMap holding the gear's YAML configuration.

The config body is supplied verbatim by the consuming chart via
`.Values.config.content` (a multi-line YAML string). It is mounted into the
container at `.Values.config.mountPath` (see the deployment template). Rendered
only when config.content is non-empty.
*/}}
{{- define "toolkit-common.configmap" -}}
{{- if .Values.config.content -}}
apiVersion: v1
kind: ConfigMap
metadata:
  name: {{ include "toolkit-common.fullname" . }}
  labels:
    {{- include "toolkit-common.labels" . | nindent 4 }}
data:
  {{ .Values.config.fileName }}: |
    {{- tpl .Values.config.content . | nindent 4 }}
{{- end -}}
{{- end -}}

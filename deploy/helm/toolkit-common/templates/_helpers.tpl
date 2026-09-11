{{/*
toolkit-common: shared name/label helpers.

These mirror the standard Helm chart conventions so every gear chart produces
consistent metadata, selectors, and service-account names.
*/}}

{{/*
Chart name (respects nameOverride).
*/}}
{{- define "toolkit-common.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Fully qualified app name (respects fullnameOverride).
*/}}
{{- define "toolkit-common.fullname" -}}
{{- if .Values.fullnameOverride }}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- $name := default .Chart.Name .Values.nameOverride }}
{{- if contains $name .Release.Name }}
{{- .Release.Name | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" }}
{{- end }}
{{- end }}
{{- end }}

{{/*
Chart label value (name-version).
*/}}
{{- define "toolkit-common.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Common labels.
*/}}
{{- define "toolkit-common.labels" -}}
helm.sh/chart: {{ include "toolkit-common.chart" . }}
{{ include "toolkit-common.selectorLabels" . }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
app.kubernetes.io/part-of: cf-gears-toolkit
{{- end }}

{{/*
Selector labels.
*/}}
{{- define "toolkit-common.selectorLabels" -}}
app.kubernetes.io/name: {{ include "toolkit-common.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{/*
Service account name. Uses the created account when serviceAccount.create is
true, otherwise the operator-provided name (defaulting to "default").
*/}}
{{- define "toolkit-common.serviceAccountName" -}}
{{- if .Values.serviceAccount.create }}
{{- default (include "toolkit-common.fullname" .) .Values.serviceAccount.name }}
{{- else }}
{{- default "default" .Values.serviceAccount.name }}
{{- end }}
{{- end }}

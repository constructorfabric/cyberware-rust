{{/*
toolkit-common: optional Ingress for the HTTP edge.

Renders only when `.Values.ingress.enabled` is true (default off). Typically
enabled just for the platform-host, which fronts the api-gateway edge: external
traffic enters through the gateway and is reverse-proxied to OoP gears by path
prefix, so gear charts don't define their own Ingress (ADR-0007).

The backend targets this chart's own Service (`toolkit-common.fullname`) on the
HTTP port (`.Values.service.port`, published as `http`).
*/}}
{{- define "toolkit-common.ingress" -}}
{{- if .Values.ingress.enabled -}}
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata:
  name: {{ include "toolkit-common.fullname" . }}
  labels:
    {{- include "toolkit-common.labels" . | nindent 4 }}
  {{- with .Values.ingress.annotations }}
  annotations:
    {{- toYaml . | nindent 4 }}
  {{- end }}
spec:
  {{- with .Values.ingress.className }}
  ingressClassName: {{ . }}
  {{- end }}
  {{- with .Values.ingress.tls }}
  tls:
    {{- toYaml . | nindent 4 }}
  {{- end }}
  rules:
    {{- $svcName := include "toolkit-common.fullname" . }}
    {{- $svcPort := .Values.service.port }}
    {{- range .Values.ingress.hosts }}
    - host: {{ .host | quote }}
      http:
        paths:
          {{- range .paths }}
          - path: {{ .path }}
            pathType: {{ .pathType | default "Prefix" }}
            backend:
              service:
                name: {{ $svcName }}
                port:
                  number: {{ $svcPort }}
          {{- end }}
    {{- end }}
{{- end -}}
{{- end -}}

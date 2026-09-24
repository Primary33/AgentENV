package compose

import (
	"context"
	"encoding/json"
	"strings"
	"testing"

	"github.com/compose-spec/compose-go/v2/loader"
	"github.com/compose-spec/compose-go/v2/types"
)

func TestIndependentServicesAndSingleInterpolation(t *testing.T) {
	t.Setenv("HOST_ONLY_SECRET", "must-not-leak")
	plan, err := Prepare(context.Background(), Request{
		Compose: `services:
  a:
    image: ${IMAGE}
    command: ["sh", "-c", "echo $$HOME ${VALUE}"]
    environment:
      PASS: ${VALUE}
      HOST_ONLY_SECRET:
  b:
    image: ${IMAGE}
`, Environment: map[string]string{"IMAGE": "busybox:1.37", "VALUE": "$literal"},
	})
	if err != nil {
		t.Fatal(err)
	}
	if len(plan.Services) != 2 || plan.Services[0].Image != plan.Services[1].Image || plan.Services[0].LocalImage == plan.Services[1].LocalImage || plan.Services[0].MountPath == plan.Services[1].MountPath {
		t.Fatalf("missing isolation: %+v", plan.Services)
	}
	guest, err := loader.LoadWithContext(context.Background(), types.ConfigDetails{
		ConfigFiles: []types.ConfigFile{{Filename: "-", Content: plan.Compose}},
		Environment: types.Mapping{"HOME": "wrong", "literal": "wrong"},
	}, func(o *loader.Options) { o.SetProjectName("aenv", true) })
	if err != nil {
		t.Fatal(err)
	}
	if got := guest.Services["a"].Command[2]; got != "echo $HOME $literal" {
		t.Fatalf("command changed: %q", got)
	}
	if got := *guest.Services["a"].Environment["PASS"]; got != "$literal" {
		t.Fatal(got)
	}
	if strings.Contains(string(plan.Compose), "must-not-leak") {
		t.Fatal("host environment leaked")
	}
}

func TestRejectExternalInputs(t *testing.T) {
	if _, err := Prepare(context.Background(), Request{Compose: "services: {app: {image: busybox}}\n---\nservices: {app: {privileged: true}}"}); err == nil {
		t.Fatal("second YAML document bypassed validation")
	}
	for _, field := range []string{"env_file: /etc/passwd", "extends: {file: /etc/passwd, service: foo}", "build: .", "deploy: {replicas: 2}", "network_mode: host", "privileged: true", "volumes: ['/etc:/host']", "volumes: [{type: bind, source: /etc, target: /host}]", "label_file: /etc/passwd"} {
		t.Run(field, func(t *testing.T) {
			_, err := Prepare(context.Background(), Request{Compose: "services:\n  app:\n    image: busybox\n    " + field + "\n"})
			if err == nil {
				t.Fatal("unsupported field accepted")
			}
		})
	}
	for _, extra := range []string{"include: /etc/passwd", "configs: {foo: {file: /etc/passwd}}", "volumes: {data: {driver_opts: {device: /etc}}}", "networks: {ext: {external: true}}"} {
		if _, err := Prepare(context.Background(), Request{Compose: "services: {app: {image: busybox}}\n" + extra}); err == nil {
			t.Fatal(extra)
		}
	}
}

func TestProfilesAndVolumes(t *testing.T) {
	plan, err := Prepare(context.Background(), Request{Compose: `services:
  app:
    image: redis:7
    volumes: [data:/data]
    ports: ['8080:80']
  debug:
    image: busybox
    profiles: [debug]
volumes:
  data: {}
`})
	if err != nil {
		t.Fatal(err)
	}
	if len(plan.Services) != 1 {
		t.Fatal(plan.Services)
	}
	var doc map[string]any
	if err := json.Unmarshal(plan.Compose, &doc); err != nil {
		t.Fatal(err)
	}
	if len(doc["services"].(map[string]any)) != 1 {
		t.Fatal(string(plan.Compose))
	}
}

// Package compose prepares an image-only Compose project for an AgentENV sandbox.
package compose

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"sort"
	"strings"

	"github.com/compose-spec/compose-go/v2/loader"
	"github.com/compose-spec/compose-go/v2/types"
	"gopkg.in/yaml.v3"
)

const MaxServices = 24

type Request struct {
	Compose     string            `json:"compose"`
	Environment map[string]string `json:"composeEnv,omitempty"`
	Profiles    []string          `json:"profiles,omitempty"`
}

type Service struct {
	Name       string `json:"name"`
	Image      string `json:"image"`
	LocalImage string `json:"localImage"`
	DriveID    string `json:"driveID"`
	MountPath  string `json:"mountPath"`
}

type Plan struct {
	Compose  json.RawMessage `json:"compose"`
	Services []Service       `json:"services"`
}

// Prepare never imports the server's environment, files, or remote resources.
// Validate the raw tree before invoking the loader, which can otherwise read
// env_file/include/extends/configs during normalization.
func Prepare(ctx context.Context, request Request) (*Plan, error) {
	if len(request.Compose) == 0 || len(request.Compose) > 1024*1024 {
		return nil, fmt.Errorf("compose must contain between 1 byte and 1 MiB")
	}
	var raw map[string]any
	decoder := yaml.NewDecoder(strings.NewReader(request.Compose))
	if err := decoder.Decode(&raw); err != nil {
		return nil, err
	}
	var extra any
	if err := decoder.Decode(&extra); err != io.EOF {
		return nil, fmt.Errorf("compose must contain exactly one YAML document")
	}
	if err := validateRaw(raw); err != nil {
		return nil, err
	}
	project, err := loader.LoadWithContext(ctx, types.ConfigDetails{
		WorkingDir: "/var/lib/agentenv-compose",
		// Pass the exact validated tree; do not parse the source twice with
		// potentially different YAML merge/document handling.
		ConfigFiles: []types.ConfigFile{{Filename: "compose.yaml", Config: raw}},
		Environment: types.Mapping(request.Environment),
	}, func(o *loader.Options) {
		o.SetProjectName("aenv", true)
		o.ResolvePaths = false
		o.SkipInclude = true
		o.SkipExtends = true
		o.SkipResolveEnvironment = true
		o.Profiles = request.Profiles
	})
	if err != nil {
		return nil, err
	}
	// env_file is rejected above, so this only resolves environment entries
	// against the explicitly supplied environment map.
	project, err = project.WithServicesEnvironmentResolved(true)
	if err != nil {
		return nil, err
	}
	if len(project.Services) == 0 || len(project.Services) > MaxServices {
		return nil, fmt.Errorf("compose must select 1..%d services", MaxServices)
	}
	plan := &Plan{}
	names := project.ServiceNames()
	sort.Strings(names)
	for i, name := range names {
		service := project.Services[name]
		if service.Image == "" {
			return nil, fmt.Errorf("services.%s.image is required", name)
		}
		if service.Platform != "" && service.Platform != "linux/amd64" {
			return nil, fmt.Errorf("services.%s.platform: only linux/amd64 is supported", name)
		}
		for _, port := range service.Ports {
			if port.Published == "" || strings.Contains(port.Published, "-") || port.Published == "0" {
				return nil, fmt.Errorf("services.%s.ports requires fixed published ports", name)
			}
			if port.Published == "49983" {
				return nil, fmt.Errorf("services.%s.ports: port 49983 is reserved for envd", name)
			}
			if port.Protocol != "tcp" && port.Protocol != "" {
				return nil, fmt.Errorf("services.%s.ports: only TCP publishing is supported", name)
			}
		}
		id := fmt.Sprintf("compose_%d", i)
		alias := fmt.Sprintf("aenv-compose/service-%d:local", i)
		plan.Services = append(plan.Services, Service{
			Name: name, Image: service.Image, LocalImage: alias,
			DriveID: id, MountPath: "/mnt/" + id,
		})
		service.Image = alias
		service.PullPolicy = "never"
		service.Profiles = nil
		project.Services[name] = service
	}
	encoded, err := project.MarshalJSON()
	if err != nil {
		return nil, err
	}
	// The guest Compose CLI interpolates its input again. Escape all literal
	// dollars after the first interpolation, including dollars introduced by
	// composeEnv, so command/healthcheck/environment retain their exact values.
	var document any
	if err := json.Unmarshal(encoded, &document); err != nil {
		return nil, err
	}
	plan.Compose, err = json.Marshal(escapeDollars(document))
	return plan, err
}

func escapeDollars(value any) any {
	switch v := value.(type) {
	case string:
		return strings.ReplaceAll(v, "$", "$$")
	case []any:
		for i := range v {
			v[i] = escapeDollars(v[i])
		}
	case map[string]any:
		for key := range v {
			v[key] = escapeDollars(v[key])
		}
	}
	return value
}

func allowedKeys(value map[string]any, path, allowed string) error {
	set := " " + allowed + " "
	for key := range value {
		if strings.HasPrefix(key, "x-") {
			continue
		}
		if !strings.Contains(set, " "+key+" ") {
			return fmt.Errorf("%s%s is not supported by sandbox Compose", path, key)
		}
	}
	return nil
}

func validateRaw(raw map[string]any) error {
	if err := allowedKeys(raw, "", "name version services networks volumes"); err != nil {
		return err
	}
	services, ok := raw["services"].(map[string]any)
	if !ok {
		return fmt.Errorf("services must be a mapping")
	}
	for name, value := range services {
		s, ok := value.(map[string]any)
		if !ok {
			return fmt.Errorf("services.%s must be a mapping", name)
		}
		if err := allowedKeys(s, "services."+name+".", "image platform command entrypoint environment user working_dir depends_on healthcheck restart ports expose networks volumes tmpfs profiles labels hostname init stop_signal stop_grace_period read_only shm_size mem_limit cpus"); err != nil {
			return err
		}
		if volumes, ok := s["volumes"].([]any); ok {
			for _, volume := range volumes {
				switch v := volume.(type) {
				case string:
					parts := strings.Split(v, ":")
					if len(parts) < 2 || strings.ContainsAny(parts[0], "/\\.$~") || parts[0] == "" {
						return fmt.Errorf("services.%s.volumes only supports named volumes", name)
					}
				case map[string]any:
					if v["type"] != "volume" && v["type"] != "tmpfs" {
						return fmt.Errorf("services.%s.volumes only supports volume or tmpfs mounts", name)
					}
				}
			}
		}
	}
	for _, section := range []string{"volumes", "networks"} {
		entries, _ := raw[section].(map[string]any)
		for name, value := range entries {
			if value == nil {
				continue
			}
			entry, ok := value.(map[string]any)
			if !ok {
				return fmt.Errorf("%s.%s must be a mapping", section, name)
			}
			allowed := "name labels driver"
			if section == "networks" {
				allowed += " internal attachable"
			}
			if err := allowedKeys(entry, section+"."+name+".", allowed); err != nil {
				return err
			}
			if driver, ok := entry["driver"]; ok {
				want := "local"
				if section == "networks" {
					want = "bridge"
				}
				if driver != want {
					return fmt.Errorf("%s.%s.driver must be %s", section, name, want)
				}
			}
		}
	}
	return nil
}

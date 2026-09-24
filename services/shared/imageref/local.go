// Package imageref recognizes AgentENV's registry-free image references.
package imageref

import "strings"

func IsLocalDigest(image string) bool {
	hex, ok := strings.CutPrefix(image, "sha256:")
	if !ok || len(hex) != 64 {
		return false
	}
	for _, c := range hex {
		if !(c >= '0' && c <= '9' || c >= 'a' && c <= 'f') {
			return false
		}
	}
	return true
}

func LocalDigests(images []string) []string {
	var result []string
	seen := make(map[string]bool)
	for _, image := range images {
		if IsLocalDigest(image) && !seen[image] {
			result = append(result, image)
			seen[image] = true
		}
	}
	return result
}

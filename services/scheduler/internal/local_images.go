package scheduler

import (
	"time"

	schedulerv1 "agentenv/services/api/proto"
	"agentenv/services/shared/imageref"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
)

// Cache inventories are rebuilt by every heartbeat, including after a scheduler
// restart. They are placement constraints, never durable image ownership.
func (r *AtomicNodeRegistry) HasLocalImages(nodeID string, digests []string, now time.Time) bool {
	r.mu.RLock()
	defer r.mu.RUnlock()
	record, ok := r.observed[nodeID]
	if !ok || r.deriveObservedNodeViewLocked(record, now.UnixMilli()).GetSnapshot().GetStatus() != schedulerv1.NodeStatus_NODE_STATUS_READY {
		return false
	}
	for _, digest := range digests {
		if _, ok := record.localImages[digest]; !ok {
			return false
		}
	}
	return true
}

func (s *Service) filterLocalImages(eligible []RichNode, digests []string) ([]RichNode, error) {
	if len(digests) == 0 {
		return eligible, nil
	}
	for _, digest := range digests {
		if !imageref.IsLocalDigest(digest) {
			return nil, status.Error(codes.InvalidArgument, "image dependencies must be sha256 manifest digests")
		}
	}
	var candidates []RichNode
	now := time.Now()
	for _, node := range eligible {
		if s.nodes.HasLocalImages(node.ID, digests, now) {
			candidates = append(candidates, node)
		}
	}
	if len(candidates) == 0 {
		return nil, status.Error(codes.Unavailable, "no available capacity has all required local images; retry later, rebuild the Compose project, or use registry images")
	}
	return candidates, nil
}

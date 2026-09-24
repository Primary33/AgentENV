package scheduler

import (
	"context"
	"strings"
	"testing"
	"time"

	schedulerv1 "agentenv/services/api/proto"
	"agentenv/services/shared/config"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
)

func TestLocalImagePlacementAndInventoryReplacement(t *testing.T) {
	a, b := "sha256:"+strings.Repeat("a", 64), "sha256:"+strings.Repeat("b", 64)
	registry := NewAtomicNodeRegistry([]Node{{ID: "a", Endpoint: "http://a"}, {ID: "b", Endpoint: "http://b"}}, time.Second)
	service := NewService(nil, registry, NewStrategy("round_robin"), NewInMemoryBindingStore(time.Second))
	report := func(node string, images []string, when time.Time) {
		t.Helper()
		_, _, err := registry.Heartbeat(&schedulerv1.HeartbeatRequest{NodeId: node, ServiceInstanceId: "instance", LocalImages: images, Snapshot: &schedulerv1.NodeSnapshot{Status: schedulerv1.NodeStatus_NODE_STATUS_READY}}, when)
		if err != nil {
			t.Fatal(err)
		}
	}
	request := &schedulerv1.ScheduleRequest{Hint: &schedulerv1.ScheduleRequestHint{RequiredImages: []string{a, b}}}
	report("a", []string{a}, time.Now())
	report("b", []string{a, b}, time.Now())
	for i := 0; i < 4; i++ {
		response, err := service.Schedule(context.Background(), request)
		if err != nil || response.GetNode().GetNodeId() != "b" {
			t.Fatalf("response=%v err=%v", response, err)
		}
	}
	// Eviction replaces the entire inventory; a union would route to missing data.
	report("b", []string{b}, time.Now())
	if _, err := service.Schedule(context.Background(), request); status.Code(err) != codes.Unavailable {
		t.Fatalf("evicted images: %v", err)
	}
	report("a", []string{a, b}, time.Now())
	response, err := service.Schedule(context.Background(), request)
	if err != nil || response.GetNode().GetNodeId() != "a" {
		t.Fatalf("new image owner: %v %v", response, err)
	}
	report("a", []string{a, b}, time.Now().Add(-time.Minute))
	if _, err := service.Schedule(context.Background(), request); status.Code(err) != codes.Unavailable {
		t.Fatalf("stale inventory: %v", err)
	}
	request.Hint.RequiredImages = []string{"node-name"}
	if _, err := service.Schedule(context.Background(), request); status.Code(err) != codes.InvalidArgument {
		t.Fatalf("invalid dependency: %v", err)
	}
}

func TestLocalImagesRequireHealthyEligibleCapacity(t *testing.T) {
	digest := "sha256:" + strings.Repeat("a", 64)
	for _, name := range []string{"no heartbeat", "unhealthy", "connecting", "lingering", "full", "restart"} {
		t.Run(name, func(t *testing.T) {
			node := Node{ID: "node", Endpoint: "http://node"}
			registry := NewAtomicNodeRegistry([]Node{node}, time.Second)
			service := NewService(nil, registry, NewStrategy("round_robin"), NewInMemoryBindingStore(time.Second), WithNodeResourceLimit(&config.NodeResourceLimit{MaxSandboxCount: uint32Ptr(1)}))
			state := schedulerv1.NodeStatus_NODE_STATUS_READY
			if name == "unhealthy" {
				state = schedulerv1.NodeStatus_NODE_STATUS_UNHEALTHY
			}
			if name == "connecting" {
				state = schedulerv1.NodeStatus_NODE_STATUS_CONNECTING
			}
			var count uint32
			if name == "full" {
				count = 2
			}
			if name != "no heartbeat" && name != "restart" {
				_, _, err := registry.Heartbeat(&schedulerv1.HeartbeatRequest{NodeId: node.ID, ServiceInstanceId: "instance", LocalImages: []string{digest}, Snapshot: &schedulerv1.NodeSnapshot{Status: state, SandboxCount: count}}, time.Now())
				if err != nil {
					t.Fatal(err)
				}
			}
			if name == "lingering" {
				registry.Set(nil, []Node{node})
			}
			request := &schedulerv1.ScheduleRequest{Hint: &schedulerv1.ScheduleRequestHint{RequiredImages: []string{digest}}}
			if _, err := service.Schedule(context.Background(), request); status.Code(err) != codes.Unavailable {
				t.Fatalf("unavailable image accepted: %v", err)
			}
			if name == "restart" {
				_, err := service.Heartbeat(context.Background(), &schedulerv1.HeartbeatRequest{NodeId: node.ID, ServiceInstanceId: "new-instance", LocalImages: []string{digest}, Snapshot: &schedulerv1.NodeSnapshot{Status: schedulerv1.NodeStatus_NODE_STATUS_READY}})
				if err != nil {
					t.Fatal(err)
				}
				if _, err := service.Schedule(context.Background(), request); err != nil {
					t.Fatalf("inventory not recovered: %v", err)
				}
			}
		})
	}
}

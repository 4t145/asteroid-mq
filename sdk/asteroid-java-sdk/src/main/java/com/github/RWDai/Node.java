package com.github.RWDai;

import java.net.URI;
import java.net.URISyntaxException;
import java.nio.ByteBuffer;
import java.util.HashMap;
import java.util.HashSet;
import java.util.List;
import java.util.Map;
import java.util.concurrent.BlockingQueue;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.LinkedBlockingQueue;
import java.util.concurrent.atomic.AtomicInteger;

import org.java_websocket.client.WebSocketClient;
import org.java_websocket.handshake.ServerHandshake;
import org.slf4j.Logger;
import org.slf4j.LoggerFactory;

import com.fasterxml.jackson.annotation.JsonInclude;
import com.fasterxml.jackson.core.JsonProcessingException;
import com.fasterxml.jackson.databind.ObjectMapper;
import com.fasterxml.jackson.databind.PropertyNamingStrategies;
import com.github.RWDai.EdgeException.NodeException;
import com.github.RWDai.Types.EdgeEndpointOnline;
import com.github.RWDai.Types.EdgeError;
import com.github.RWDai.Types.EdgeMessage;
import com.github.RWDai.Types.EdgePayload;
import com.github.RWDai.Types.EdgePushPayload;
import com.github.RWDai.Types.EdgeRequest;
import com.github.RWDai.Types.EdgeRequestEnum;
import com.github.RWDai.Types.EdgeResponseEnum;
import com.github.RWDai.Types.EdgeResponsePayload;
import com.github.RWDai.Types.EdgeResult;
import com.github.RWDai.Types.MessageAck;
import com.github.RWDai.Types.MessagePush;
import com.github.RWDai.Types.MessageStateUpdate;
import com.github.RWDai.Types.SendMessageRequest;
import com.github.RWDai.Types.SetState;
import com.github.RWDai.Types.SetStateRequest;

public class Node implements AutoCloseable {
  private static ObjectMapper objectMapper = new ObjectMapper()
      .setPropertyNamingStrategy(PropertyNamingStrategies.SNAKE_CASE)
      .setSerializationInclusion(JsonInclude.Include.NON_NULL);
  private static final Logger log = LoggerFactory.getLogger(Node.class);

  private WebSocketClient socket;
  private CountDownLatch socketConnectLatch = new CountDownLatch(1);;
  private AtomicInteger requestId = new AtomicInteger(0);
  private BlockingQueue<BoxRequest> requestPool = new LinkedBlockingQueue<>();
  private boolean alive = false;
  private Map<String, Endpoint> endpoints = new HashMap<>();

  private Node() {
  }

  public boolean isAlive() {
    try {
      socketConnectLatch.await();
    } catch (InterruptedException e) {
      throw new NodeException("Failed to connect", e);
    }
    return alive;
  }

  private void setAlive(boolean alive) {
    this.alive = alive;
  }

  protected BlockingQueue<BoxRequest> getRequestPool() {
    return this.requestPool;
  }

  public static class BoxRequest {
    private EdgeRequest request;
    private BlockingQueue<EdgeResult<EdgeResponseEnum, EdgeError>> responseQueue;

    public EdgeRequest getRequest() {
      return request;
    }

    public void setRequest(EdgeRequest request) {
      this.request = request;
    }

    public BlockingQueue<EdgeResult<EdgeResponseEnum, EdgeError>> getResponseQueue() {
      return responseQueue;
    }

    public void setResponseQueue(BlockingQueue<EdgeResult<EdgeResponseEnum, EdgeError>> responseQueue) {
      this.responseQueue = responseQueue;
    }
  }

  protected Map<String, Endpoint> getEndpoints() {
    return endpoints;
  }

  public static Node connect(String url) {
    Node node = new Node();
    var responsePool = new HashMap<Integer, BlockingQueue<EdgeResult<EdgeResponseEnum, EdgeError>>>();
    try {
      WebSocketClient socket = new WebSocketClient(new URI(url)) {
        @Override
        public void onOpen(ServerHandshake handshakedata) {
          node.setAlive(true);
          node.socketConnectLatch.countDown();
        }

        public void onMessage(ByteBuffer bytes) {
          String message = new String(bytes.array());
          onMessage(message);
        }

        @Override
        public void onMessage(String message) {
          // 实现 onMessage 逻辑
          try {
            EdgePayload payload = objectMapper.readValue(message, EdgePayload.class);
            if (payload instanceof EdgeResponsePayload) {
              var result = ((EdgeResponsePayload) payload).getContent().getResult();
              var seqId = ((EdgeResponsePayload) payload).getContent().getSeqId();
              var responseQueue = responsePool.get(seqId);
              if (responseQueue != null) {
                responseQueue.put(result);
              }
            } else if (payload instanceof EdgePushPayload) {
              var content = ((EdgePushPayload) payload).getContent();
              if (content instanceof MessagePush) {
                var contentMessage = ((MessagePush) content).getMessage();
                var contentEndpoints = ((MessagePush) content).getEndpoints();

                for (String endpointName : contentEndpoints) {
                  var endpoint = node.endpoints.get(endpointName);
                  if (endpoint == null) {
                    continue;
                  }
                  endpoint.getMessageQueue().put(contentMessage);
                }
              } else {
                log.warn("socket onMessage unknown payload:{}", payload);
              }
            } else {
              log.warn("socket onMessage unknown payload:{}", payload);
            }
          } catch (InterruptedException | JsonProcessingException e) {
            log.warn("socket onMessage error", e);
          }

        }

        @Override
        public void onClose(int code, String reason, boolean remote) {
          node.setAlive(false);
        }

        @Override
        public void onError(Exception ex) {
          this.close();
          node.setAlive(false);
          log.warn("socket onError", ex);
        }
      };
      socket.connect();

      Thread.startVirtualThread(() -> {
        while (true) {
          try {
            var boxRequest = node.getRequestPool().take();
            var seq_id = boxRequest.getRequest().getSeqId();
            var message = objectMapper.writeValueAsString(new Types.EdgeRequestPayload(boxRequest.getRequest()));
            socket.send(message.getBytes());
            responsePool.put(seq_id, boxRequest.getResponseQueue());
          } catch (InterruptedException | JsonProcessingException e) {
            log.warn("socket sendRequest error", e);
          }
        }
      });
      node.socket = socket;
      return node;
    } catch (URISyntaxException e) {
      throw new NodeException("Failed to connect", e);
    }
  }

  public EdgeResult<EdgeResponseEnum, EdgeError> sendMessage(EdgeMessage message) throws InterruptedException {
    if (!isAlive()) {
      throw new NodeException("Node is not alive");
    }
    return sendRequest(new SendMessageRequest(message));
  }

  public Endpoint createEndpoint(String topicCode, List<String> interests) throws InterruptedException {
    if (!isAlive()) {
      throw new NodeException("Node is not alive");
    }
    var address = sendEndpointsOnline(new EdgeEndpointOnline(topicCode, interests));
    var endpoint = new Endpoint(this, topicCode, new HashSet<>(interests), address);
    endpoints.put(address, endpoint);
    return endpoint;
  }

  private EdgeResult<EdgeResponseEnum, EdgeError> sendRequest(EdgeRequestEnum request) throws InterruptedException {
    var responseQueue = new LinkedBlockingQueue<EdgeResult<EdgeResponseEnum, EdgeError>>();
    BoxRequest boxRequest = new BoxRequest();
    boxRequest.setRequest(new EdgeRequest(nextRequestId(), request));
    boxRequest.setResponseQueue(responseQueue);
    requestPool.put(boxRequest);
    var result = responseQueue.take();
    return result;
  }

  private int nextRequestId() {
    return requestId.incrementAndGet();
  }

  protected void sendSingleAck(MessageAck ack) throws InterruptedException {
    var response = sendRequest(
        new SetStateRequest(new SetState(ack.getTopicCode(), new MessageStateUpdate(ack.getAckTo(),
            Map.of(ack.getFrom(), ack.getKind())))));
    if (response instanceof Types.Ok) {
      @SuppressWarnings("rawtypes")
      var content = ((Types.Ok) response).getContent();
      if (content instanceof Types.SetStateResponse) {
        return;
      }
    }
    throw new EdgeException.UnknownResponseException("Unknown response type");
  }

  private String sendEndpointsOnline(EdgeEndpointOnline request) throws InterruptedException {
    var response = sendRequest(new Types.EndpointOnlineRequest(request));
    if (response instanceof Types.Ok) {
      var content = ((Types.Ok) response).getContent();
      if (content instanceof Types.EndpointOnlineResponse) {
        return ((Types.EndpointOnlineResponse) content).getContent();
      }
    }
    throw new EdgeException.UnknownResponseException("Unknown response type");
  }

  protected void sendEndpointsOffline(Types.EdgeEndpointOffline request) throws InterruptedException {
    var response = sendRequest(new Types.EndpointOfflineRequest(request));
    if (response instanceof Types.Ok) {
      var content = ((Types.Ok) response).getContent();
      if (content instanceof Types.EndpointOfflineResponse) {
        return;
      }
    }
    throw new EdgeException.UnknownResponseException("Unknown response type");
  }

  protected void sendEndpointsInterests(Types.EndpointInterestRequest request) throws InterruptedException {
    var response = sendRequest(request);
    if (response instanceof Types.Ok) {
      var content = ((Types.Ok) response).getContent();
      if (content instanceof Types.EndpointInterestResponse) {
        return;
      }
    }
    throw new EdgeException.UnknownResponseException("Unknown response type");
  }

  @Override
  public void close() {
    // 实现关闭逻辑
    for (Endpoint ep : this.endpoints.values()) {
      ep.close();
    }
    socket.close();
  }
}

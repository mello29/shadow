/*
 * The Shadow Simulator
 * Copyright (c) 2010-2011, Rob Jansen
 * See LICENSE for licensing information
 */

#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif

#if !defined __USE_LARGEFILE64
#define __USE_LARGEFILE64
#endif

#include <errno.h>
#include <glib.h>
#include <glib/gstdio.h>
#include <stddef.h>
#include <unistd.h>

#include "lib/logger/log_level.h"
#include "lib/logger/logger.h"
#include "main/core/controller.h"
#include "main/core/manager.h"
#include "main/core/support/definitions.h"
#include "main/host/host.h"
#include "main/routing/address.h"
#include "main/routing/dns.h"
#include "main/utility/random.h"
#include "main/utility/utility.h"

struct _Controller {
    /* general options and user configuration for the simulation */
    ConfigOptions* config;

    /* tracks overall wall-clock runtime */
    GTimer* runTimer;

    /* global random source from which all node random sources originate */
    Random* random;

    /* global network connectivity info */
    NetworkGraph* graph;
    IpAssignment_u32* ipAssignment;
    RoutingInfo_u32* routingInfo;
    DNS* dns;

    /* minimum allowed time jump when sending events between nodes */
    SimulationTime minJumpTimeConfig;
    SimulationTime minJumpTime;
    SimulationTime nextMinJumpTime;

    /* start of current window of execution */
    SimulationTime executeWindowStart;
    /* end of current window of execution (start + min_time_jump) */
    SimulationTime executeWindowEnd;
    /* the simulator should attempt to end immediately after this time */
    SimulationTime endTime;

    /* if we run in unlimited bandwidth mode, this is when we go back to bw enforcement */
    SimulationTime bootstrapEndTime;

    Manager* manager;

    MAGIC_DECLARE;
};

// TODO
// static gboolean _controller_handleInterruptSignal(controller* controller) {
//  MAGIC_ASSERT(controller);
//
//  /* handle (SIGHUP, SIGTERM, SIGINT), shutdown cleanly */
//  controller->endTime = 0;
//  controller->killed = TRUE;
//
//  /* dont remove the source */
//  return FALSE;
//}

Controller* controller_new(ConfigOptions* config) {
    utility_assert(config);

    /* Don't do anything in this function that will cause a log message. The
     * global engine is still NULL since we are creating it now, and logging
     * here will cause an assertion error.
     */

    Controller* controller = g_new0(Controller, 1);
    MAGIC_INIT(controller);

    controller->config = config;
    controller->random = random_new(config_getSeed(config));

    controller->minJumpTimeConfig = config_getRunahead(config);

    /* these are only avail in glib >= 2.30
     * setup signal handlers for gracefully handling shutdowns */
    //  TODO
    //  g_unix_signal_add(SIGTERM, (GSourceFunc)_controller_handleInterruptSignal, controller);
    //  g_unix_signal_add(SIGHUP, (GSourceFunc)_controller_handleInterruptSignal, controller);
    //  g_unix_signal_add(SIGINT, (GSourceFunc)_controller_handleInterruptSignal, controller);

    info("simulation controller created");
    return controller;
}

void controller_free(Controller* controller) {
    MAGIC_ASSERT(controller);

    if (controller->routingInfo) {
        routinginfo_free(controller->routingInfo);
        controller->routingInfo = NULL;
    }
    if (controller->ipAssignment) {
        ipassignment_free(controller->ipAssignment);
        controller->ipAssignment = NULL;
    }
    if (controller->graph) {
        // this should have been freed earlier when we were done with it
        warning("network graph was not properly freed");
        networkgraph_free(controller->graph);
        controller->graph = NULL;
    }
    if (controller->dns) {
        dns_free(controller->dns);
        controller->dns = NULL;
    }
    if (controller->random) {
        random_free(controller->random);
        controller->random = NULL;
    }

    MAGIC_CLEAR(controller);
    g_free(controller);

    info("simulation controller destroyed");
}

static SimulationTime _controller_getMinTimeJump(Controller* controller) {
    MAGIC_ASSERT(controller);

    /* use minimum network latency of our topology
     * if not yet computed, default to 10 milliseconds */
    SimulationTime minJumpTime =
        controller->minJumpTime > 0 ? controller->minJumpTime : 10 * SIMTIME_ONE_MILLISECOND;

    /* if the command line option was given, use that as lower bound */
    if (controller->minJumpTimeConfig > 0 && minJumpTime < controller->minJumpTimeConfig) {
        minJumpTime = controller->minJumpTimeConfig;
    }

    return minJumpTime;
}

void controller_updateMinTimeJump(Controller* controller, gdouble minPathLatency) {
    MAGIC_ASSERT(controller);
    SimulationTime minPathLatencySimTime = ((SimulationTime)minPathLatency) * SIMTIME_ONE_MILLISECOND;

    if (controller->nextMinJumpTime == 0 || minPathLatencySimTime < controller->nextMinJumpTime) {
        utility_assert(minPathLatencySimTime > 0);
        SimulationTime oldJumpNs = controller->nextMinJumpTime;
        controller->nextMinJumpTime = minPathLatencySimTime;
        debug("updated topology minimum time jump from %" G_GUINT64_FORMAT " to %" G_GUINT64_FORMAT
              " nanoseconds; "
              "the minimum config override is %s (%" G_GUINT64_FORMAT " nanoseconds)",
              oldJumpNs, controller->nextMinJumpTime,
              controller->minJumpTimeConfig > 0 ? "set" : "not set", controller->minJumpTimeConfig);
    }
}

static gboolean _controller_loadNetworkGraph(Controller* controller) {
    MAGIC_ASSERT(controller);

    controller->graph = networkgraph_load(controller->config);
    controller->ipAssignment = ipassignment_new();

    if (!controller->graph) {
        error("fatal error loading graph, check your syntax and try again");
        return FALSE;
    }

    /* initialize global DNS addressing */
    controller->dns = dns_new();
    return TRUE;
}

static void _controller_initializeTimeWindows(Controller* controller) {
    MAGIC_ASSERT(controller);

    /* set simulation end time */
    controller->endTime = config_getStopTime(controller->config);

    /* simulation mode depends on configured number of workers */
    guint nWorkers = config_getWorkers(controller->config);
    if (nWorkers > 0) {
        /* multi threaded, manage the other workers */
        controller->executeWindowStart = 0;
        SimulationTime jump = _controller_getMinTimeJump(controller);
        controller->executeWindowEnd = jump;
    } else {
        /* single threaded, we are the only worker */
        controller->executeWindowStart = 0;
        controller->executeWindowEnd = G_MAXUINT64;
    }

    controller->bootstrapEndTime = config_getBootstrapEndTime(controller->config);
}

static void _controller_registerArgCallback(const char* arg, void* _argArray) {
    GPtrArray* argArray = _argArray;

    char* copiedArg = strdup(arg);
    utility_assert(copiedArg != NULL);

    g_ptr_array_add(argArray, copiedArg);
}

typedef struct _ProcessCallbackArgs {
    Controller* controller;
    const char* hostname;
} ProcessCallbackArgs;

__attribute__((warn_unused_result))
static int _controller_registerProcessCallback(const ProcessOptions* proc, void* _callbackArgs) {
    ProcessCallbackArgs* callbackArgs = _callbackArgs;

    char* plugin = processoptions_getPath(proc);
    if (plugin == NULL) {
        error("For host '%s', couldn't find program path: '%s'", callbackArgs->hostname,
              processoptions_getRawPath(proc));
        return -1;
    }

    // build an argv array
    GPtrArray* argArray = g_ptr_array_new();
    g_ptr_array_add(argArray, strdup(plugin));

    // iterate through the arguments and copy them to our array
    processoptions_getArgs(proc, _controller_registerArgCallback, (void*)argArray);

    // the last element of argv must be NULL
    g_ptr_array_add(argArray, NULL);

    // free the GLib array but keep the data
    gchar** argv = (gchar**)g_ptr_array_free(argArray, FALSE);

    guint64 quantity = processoptions_getQuantity(proc);

    char* environment = processoptions_getEnvironment(proc);

    for (guint64 i = 0; i < quantity; i++) {
        manager_addNewVirtualProcess(callbackArgs->controller->manager, callbackArgs->hostname,
                                     plugin, processoptions_getStartTime(proc),
                                     processoptions_getStopTime(proc), argv, environment);
    }

    processoptions_freeString(environment);
    processoptions_freeString(plugin);
    g_strfreev(argv);

    return 0;
}

typedef struct RegisterHostCallbackOptions {
    Controller* controller;
    bool registerIfAddressSpecified;
} RegisterHostCallbackOptions;

__attribute__((warn_unused_result))
static int _controller_registerHostCallback(const char* name, const ConfigOptions* config,
                                            const HostOptions* host, void* _callbackOptions) {
    RegisterHostCallbackOptions* callbackOptions = _callbackOptions;
    Controller* controller = callbackOptions->controller;

    MAGIC_ASSERT(controller);
    utility_assert(host);

    guint managerCpuFreq = manager_getRawCPUFrequency(controller->manager);

    guint64 quantity = hostoptions_getQuantity(host);
    in_addr_t ipAddr = 0;
    bool ipAddrSet = (hostoptions_getIpAddr(host, &ipAddr) == 0);

    if (ipAddrSet != callbackOptions->registerIfAddressSpecified) {
        // skip this host
        return 0;
    }

    // make sure we're not trying to set a single address for multiple hosts
    if (ipAddrSet && quantity > 1) {
        error("Host %s has an IP address set with a quantity %ld greater than 1", name, quantity);
        return -1;
    }

    for (guint64 i = 0; i < quantity; i++) {
        HostParameters* params = g_new0(HostParameters, 1);

        GString* hostnameBuffer = g_string_new(name);
        if (quantity > 1) {
            g_string_append_printf(hostnameBuffer, "%" G_GUINT64_FORMAT, i + 1);
        }

        // the network graph node to assign the host to
        uint graphNode = hostoptions_getNetworkNodeId(host);

        if (ipAddrSet) {
            if (ipassignment_assignHostWithIp(controller->ipAssignment, graphNode, ipAddr)) {
                error("Could not register host %s", name);
                return -1;
            }
        } else {
            if (ipassignment_assignHost(controller->ipAssignment, graphNode, &ipAddr)) {
                error("Could not register host %s", name);
                return -1;
            }
        }

        params->hostname = hostnameBuffer->str;

        params->cpuFrequency = MAX(0, managerCpuFreq);
        params->cpuThreshold = 0;
        params->cpuPrecision = 200;

        params->ipAddr = ipAddr;

        params->logLevel = hostoptions_getLogLevel(host);
        params->heartbeatLogLevel = hostoptions_getHeartbeatLogLevel(host);
        params->heartbeatLogInfo = hostoptions_getHeartbeatLogInfo(host);
        params->heartbeatInterval = hostoptions_getHeartbeatInterval(host);
        params->pcapDir = hostoptions_getPcapDirectory(host);

        /* some options come from the config options and not the host options */
        params->sendBufSize = config_getSocketSendBuffer(config);
        params->recvBufSize = config_getSocketRecvBuffer(config);
        params->autotuneSendBuf = config_getSocketSendAutotune(config);
        params->autotuneRecvBuf = config_getSocketRecvAutotune(config);
        params->interfaceBufSize = config_getInterfaceBuffer(config);
        params->qdisc = config_getInterfaceQdisc(config);

        /* bandwidth values come from the host options and graph options */
        bool foundBwDown = false;
        bool foundBwUp = false;

        foundBwDown |= (0 == networkgraph_nodeBandwidthDownBits(
                                 controller->graph, graphNode, &params->requestedBwDownBits));
        foundBwDown |= (0 == hostoptions_getBandwidthDown(host, &params->requestedBwDownBits));

        foundBwUp |= (0 == networkgraph_nodeBandwidthUpBits(
                               controller->graph, graphNode, &params->requestedBwUpBits));
        foundBwUp |= (0 == hostoptions_getBandwidthUp(host, &params->requestedBwUpBits));

        if (!foundBwDown) {
            error("No downstream bandwidth provided for host %s", params->hostname);
            return -1;
        }

        if (!foundBwUp) {
            error("No upstream bandwidth provided for host %s", params->hostname);
            return -1;
        }

        if (params->requestedBwDownBits == 0 || params->requestedBwUpBits == 0) {
            error("Bandwidth for host %s must be non-zero", params->hostname);
            return -1;
        }

        /* add the host */
        manager_addNewVirtualHost(controller->manager, params);

        /* now handle each virtual process the host will run */
        ProcessCallbackArgs processArgs;
        processArgs.controller = controller;
        processArgs.hostname = hostnameBuffer->str;
        if (hostoptions_iterProcesses(
                host, _controller_registerProcessCallback, (void*)&processArgs)) {
            error("Could not register processes for host %s", name);
            return -1;
        }

        /* cleanup for next pass through the loop */
        g_string_free(hostnameBuffer, TRUE);
        hostoptions_freeString(params->pcapDir);
        g_free(params);
    }

    // no error
    return 0;
}

__attribute__((warn_unused_result))
static int _controller_registerHosts(Controller* controller) {
    MAGIC_ASSERT(controller);

    RegisterHostCallbackOptions options;

    // register hosts that have a specific IP address
    options = (RegisterHostCallbackOptions){
        .controller = controller,
        .registerIfAddressSpecified = true,
    };
    if (config_iterHosts(controller->config, _controller_registerHostCallback, (void*)&options)) {
        error("Could not register hosts with specific IP addresses");
        return -1;
    }

    // register remaining hosts
    options = (RegisterHostCallbackOptions){
        .controller = controller,
        .registerIfAddressSpecified = false,
    };
    if (config_iterHosts(controller->config, _controller_registerHostCallback, (void*)&options)) {
        error("Could not register remaining hosts");
        return -1;
    }

    return 0;
}

gint controller_run(Controller* controller) {
    MAGIC_ASSERT(controller);

    info("loading and initializing simulation data");

    gboolean isSuccess = _controller_loadNetworkGraph(controller);
    if (!isSuccess) {
        return 1;
    }

    _controller_initializeTimeWindows(controller);

    /* the controller will be responsible for distributing the actions to the managers so that
     * they all have a consistent view of the simulation, topology, etc.
     * For now we only have one manager so send it everything. */
    guint managerSeed = random_nextUInt(controller->random);
    controller->manager = manager_new(controller, controller->config, controller->endTime,
                                      controller->bootstrapEndTime, managerSeed);

    if (controller->manager == NULL) {
        utility_panic("unable to create manager");
    }

    info("registering plugins and hosts");

    /* register the components needed by each manager.
     * this must be done after managers are available so we can send them messages */
    if (_controller_registerHosts(controller)) {
        error("Unable to register hosts");
        return 1;
    }

    /* now that we know which graph nodes are in use, we can compute shortest paths */
    bool useShortestPath = config_getUseShortestPath(controller->config);
    controller->routingInfo =
        routinginfo_new(controller->graph, controller->ipAssignment, useShortestPath);
    if (controller->routingInfo == NULL) {
        error("Unable to generate topology");
        return 1;
    }

    /* we don't need the network graph anymore, so free it to save memory */
    networkgraph_free(controller->graph);
    controller->graph = NULL;

    info("running simulation");

    /* dont buffer log messages in trace mode */
    if (config_getLogLevel(controller->config) != LOGLEVEL_TRACE) {
        info("log message buffering is enabled for efficiency");
        shadow_logger_setEnableBuffering(TRUE);
    }

    /* start running each manager */
    manager_run(controller->manager);

    /* only need to disable buffering if it was enabled, otherwise
     * don't log the message as it may confuse the user. */
    if (config_getLogLevel(controller->config) != LOGLEVEL_TRACE) {
        info("log message buffering is disabled during cleanup");
        shadow_logger_setEnableBuffering(FALSE);
    }

    info("simulation finished, cleaning up now");

    return manager_free(controller->manager);
}

gboolean controller_managerFinishedCurrentRound(Controller* controller,
                                                SimulationTime minNextEventTime,
                                                SimulationTime* executeWindowStart,
                                                SimulationTime* executeWindowEnd) {
    MAGIC_ASSERT(controller);
    utility_assert(executeWindowStart && executeWindowEnd);

    /* TODO: once we get multiple managers, we have to block them here
     * until they have all notified us that they are finished */

    /* update our detected min jump time */
    if (controller->nextMinJumpTime != 0) {
        controller->minJumpTime = controller->nextMinJumpTime;
    }

    /* update the next interval window based on next event times */
    SimulationTime newStart = minNextEventTime;
    SimulationTime newEnd = minNextEventTime + _controller_getMinTimeJump(controller);

    /* update the new window end as one interval past the new window start,
     * making sure we dont run over the experiment end time */
    if (newEnd > controller->endTime) {
        newEnd = controller->endTime;
    }

    /* finally, set the new values */
    controller->executeWindowStart = newStart;
    controller->executeWindowEnd = newEnd;

    *executeWindowStart = controller->executeWindowStart;
    *executeWindowEnd = controller->executeWindowEnd;

    /* return TRUE if we should keep running */
    return newStart < newEnd ? TRUE : FALSE;
}

gdouble controller_getLatency(Controller* controller, Address* srcAddress, Address* dstAddress) {
    MAGIC_ASSERT(controller);
    // shadow uses latency in milliseconds
    return routinginfo_getLatencyNs(controller->routingInfo, controller->ipAssignment,
                                    htonl(address_toHostIP(srcAddress)),
                                    htonl(address_toHostIP(dstAddress))) /
           1000000.0;
}

gfloat controller_getReliability(Controller* controller, Address* srcAddress, Address* dstAddress) {
    MAGIC_ASSERT(controller);
    return routinginfo_getReliability(controller->routingInfo, controller->ipAssignment,
                                      htonl(address_toHostIP(srcAddress)),
                                      htonl(address_toHostIP(dstAddress)));
}

bool controller_isRoutable(Controller* controller, Address* srcAddress, Address* dstAddress) {
    MAGIC_ASSERT(controller);
    return routinginfo_isRoutable(controller->ipAssignment, htonl(address_toHostIP(srcAddress)),
                                  htonl(address_toHostIP(dstAddress)));
}

void controller_incrementPacketCount(Controller* controller, Address* srcAddress,
                                     Address* dstAddress) {
    MAGIC_ASSERT(controller);
    routinginfo_incrementPacketCount(controller->routingInfo, controller->ipAssignment,
                                     htonl(address_toHostIP(srcAddress)),
                                     htonl(address_toHostIP(dstAddress)));
}

DNS* controller_getDNS(Controller* controller) {
    MAGIC_ASSERT(controller);
    return controller->dns;
}

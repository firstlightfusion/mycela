// Custom Tooltip System
(function() {
    'use strict';
    
    let tooltip = null;
    let isPinned = false;
    let autoHideTimer = null;
    let currentTargetElement = null;
    let showTimer = null;
    
    // Create tooltip element
    function createTooltip() {
        tooltip = document.createElement('div');
        tooltip.className = 'custom-tooltip';
        tooltip.style.position = 'absolute';
        tooltip.style.pointerEvents = 'auto';
        tooltip.style.opacity = '0';
        tooltip.style.transition = 'opacity 0.2s ease';
        tooltip.style.zIndex = '10000';
        tooltip.style.cursor = 'pointer';
        document.body.appendChild(tooltip);
        
        // Click to pin/unpin
        tooltip.addEventListener('click', function(e) {
            e.stopPropagation();
            togglePin();
        });
        
        // Keep tooltip visible when hovering over it
        tooltip.addEventListener('mouseenter', function() {
            clearAutoHideTimer();
        });
        
        tooltip.addEventListener('mouseleave', function() {
            if (!isPinned) {
                startAutoHideTimer();
            }
        });
    }
    
    // Toggle pin state
    function togglePin() {
        isPinned = !isPinned;
        const pinIndicator = tooltip.querySelector('.tooltip-pin-indicator');
        
        if (isPinned) {
            tooltip.style.borderColor = 'var(--accent)';
            tooltip.style.borderWidth = '2px';
            if (pinIndicator) {
                pinIndicator.style.opacity = '1';
                pinIndicator.style.color = 'var(--accent)';
            }
            clearAutoHideTimer();
        } else {
            tooltip.style.borderColor = 'rgba(255, 255, 255, 0.2)';
            tooltip.style.borderWidth = '1px';
            if (pinIndicator) {
                pinIndicator.style.opacity = '0.4';
                pinIndicator.style.color = 'rgba(255, 255, 255, 0.7)';
            }
            startAutoHideTimer();
        }
    }
    
    // Start auto-hide timer
    function startAutoHideTimer() {
        clearAutoHideTimer();
        if (!isPinned) {
            autoHideTimer = setTimeout(() => {
                // Double-check tooltip isn't being hovered before hiding
                if (!isPinned && tooltip && !tooltip.matches(':hover')) {
                    tooltip.style.opacity = '0';
                    
                    // Don't restore title attributes yet - they'll be restored on mouseout
                    
                    setTimeout(() => {
                        if (tooltip && !isPinned && !tooltip.matches(':hover')) {
                            tooltip.style.display = 'none';
                        }
                    }, 200);
                }
            }, 3000); // Increased to 3 seconds
        }
    }
    
    // Clear auto-hide timer
    function clearAutoHideTimer() {
        if (autoHideTimer) {
            clearTimeout(autoHideTimer);
            autoHideTimer = null;
        }
    }
    
    // Clear show timer
    function clearShowTimer() {
        if (showTimer) {
            clearTimeout(showTimer);
            showTimer = null;
        }
    }
    
    // Position tooltip
    function positionTooltip(e, targetElement) {
        if (!tooltip) return;
        
        const padding = 10;
        
        // Get the element's bounding rectangle
        const elementRect = targetElement.getBoundingClientRect();
        
        // Get tooltip dimensions
        const tooltipRect = tooltip.getBoundingClientRect();
        const viewportWidth = window.innerWidth;
        
        // Position below the widget
        let left = elementRect.left;
        let top = elementRect.bottom + padding;
        
        // Horizontal positioning - keep within viewport
        if (left + tooltipRect.width > viewportWidth) {
            // Align to right edge of element if tooltip is too wide
            left = Math.max(padding, elementRect.right - tooltipRect.width);
        }
        
        // Ensure we don't go off the left edge
        left = Math.max(padding, left);
        
        tooltip.style.left = left + window.scrollX + 'px';
        tooltip.style.top = top + window.scrollY + 'px';
    }
    
    // ── Tooltip panel ──────────────────────────────────────────────────────

    // Show tooltip
    function showTooltip(e) {
        // Search up the DOM tree for title / data-tooltip / data-original-title
        let element = e.target;
        let title = null;
        
        while (element && !title) {
            title = element.getAttribute('title') || 
                    element.getAttribute('data-tooltip') || 
                    element.getAttribute('data-original-title');
            if (title) break;
            element = element.parentElement;
        }
        
        if (!title || !element) return;
        e.target = element; // Use the element with the title
        currentTargetElement = element; // Store for repositioning
        
        // Store original title if not already stored
        if (element.hasAttribute('title')) {
            element.setAttribute('data-original-title', title);
            element.removeAttribute('title');
        }
        
        if (!tooltip) createTooltip();
        
        // Create content structure with pin indicator
        tooltip.innerHTML = '';
        
        const contentDiv = document.createElement('div');
        contentDiv.textContent = title;
        
        // Material Design pin icon (push pin)
        const pinIndicator = document.createElement('div');
        pinIndicator.className = 'tooltip-pin-indicator';
        pinIndicator.innerHTML = '<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="currentColor"><path d="M16 9V4h1c.55 0 1-.45 1-1s-.45-1-1-1H7c-.55 0-1 .45-1 1s.45 1 1 1h1v5c0 1.66-1.34 3-3 3v2h5.97v7l1 1 1-1v-7H19v-2c-1.66 0-3-1.34-3-3z"/></svg>';
        pinIndicator.style.position = 'absolute';
        pinIndicator.style.top = '4px';
        pinIndicator.style.right = '4px';
        pinIndicator.style.opacity = '0.4';
        pinIndicator.style.cursor = 'pointer';
        pinIndicator.style.color = 'rgba(255, 255, 255, 0.7)';
        pinIndicator.style.transition = 'opacity 0.2s, color 0.2s';
        pinIndicator.style.padding = '2px';
        pinIndicator.style.display = 'flex';
        pinIndicator.style.alignItems = 'center';
        pinIndicator.style.justifyContent = 'center';
        
        // Hover effect for pin icon
        pinIndicator.addEventListener('mouseenter', function() {
            if (!isPinned) {
                this.style.opacity = '0.8';
            }
        });
        pinIndicator.addEventListener('mouseleave', function() {
            if (!isPinned) {
                this.style.opacity = '0.4';
            }
        });
        
        tooltip.appendChild(pinIndicator);
        tooltip.appendChild(contentDiv);
        
        tooltip.style.opacity = '0';
        tooltip.style.display = 'block';
        tooltip.style.borderColor = 'rgba(255, 255, 255, 0.2)';
        tooltip.style.borderWidth = '1px';
        isPinned = false;
        
        // Position after display to get correct dimensions
        requestAnimationFrame(() => {
            positionTooltip(e, element);
            tooltip.style.opacity = '1';
            startAutoHideTimer();
        });
    }
    
    function initTooltips() {
        let currentTooltipElement = null;

        // ── Hover tooltips for normal [title] elements ──────────────────────
        document.addEventListener('mouseover', function(e) {
            const target = e.target.closest('[title], [data-tooltip], [data-original-title]');
            if (!target || target === currentTooltipElement) return;
            // Skip elements that use click-only tooltips
            if (target.hasAttribute('data-tooltip-on-click')) return;

            currentTooltipElement = target;
            clearAutoHideTimer();
            clearShowTimer();

            const title = target.getAttribute('title');
            if (title) {
                target.setAttribute('data-original-title', title);
                target.removeAttribute('title');
            }

            showTimer = setTimeout(() => {
                showTooltip({ target, clientX: e.clientX, clientY: e.clientY });
            }, 1500);
        });

        document.addEventListener('mousemove', function(e) {
            const target = e.target.closest('[title], [data-tooltip], [data-original-title]');
            if (target === currentTooltipElement && showTimer) {
                clearShowTimer();
                showTimer = setTimeout(() => {
                    showTooltip({ target, clientX: e.clientX, clientY: e.clientY });
                }, 1500);
            }
        });

        document.addEventListener('mouseout', function(e) {
            const target = e.target.closest('[title], [data-tooltip], [data-original-title]');
            if (!target || target !== currentTooltipElement) return;
            if (target.contains(e.relatedTarget)) return;
            if (tooltip && (e.relatedTarget === tooltip || tooltip.contains(e.relatedTarget))) return;
            if (isPinned) return;

            clearShowTimer();
            const originalTitle = target.getAttribute('data-original-title');
            if (originalTitle) {
                target.setAttribute('title', originalTitle);
                target.removeAttribute('data-original-title');
            }
            currentTooltipElement = null;

            setTimeout(() => {
                if (tooltip && !tooltip.matches(':hover') && !isPinned) startAutoHideTimer();
            }, 300);
        });

        // ── Click handler ─────────────────────────────────────────────────
        document.addEventListener('click', function(e) {
            // Info button: open tooltip panel directly
            const infoBtn = e.target.closest('.widget-info-btn');
            if (infoBtn) {
                e.stopPropagation();
                showTooltip({ target: infoBtn });
                return;
            }

            // Dismiss pinned tooltip when clicking outside it
            if (tooltip && isPinned && !tooltip.contains(e.target)) {
                isPinned = false;
                tooltip.style.borderColor = 'rgba(255, 255, 255, 0.2)';
                tooltip.style.borderWidth = '1px';
                tooltip.style.opacity = '0';
                document.querySelectorAll('[data-original-title]').forEach(el => {
                    const t = el.getAttribute('data-original-title');
                    if (t) { el.setAttribute('title', t); el.removeAttribute('data-original-title'); }
                });
                currentTooltipElement = null;
                setTimeout(() => { if (tooltip && !isPinned) tooltip.style.display = 'none'; }, 200);
            }
        });
    }
    
    // Initialize when DOM is ready
    if (document.readyState === 'loading') {
        document.addEventListener('DOMContentLoaded', initTooltips);
    } else {
        initTooltips();
    }

    // Backs the "Hide Info" button: `body.info-off` hides every `.widget-info-btn`.
    window.toggleInfo = function () {
        var hidden = document.body.classList.toggle('info-off');
        var label = document.getElementById('info-lbl');
        if (label) label.textContent = hidden ? 'Show Info' : 'Hide Info';
    };
})();
